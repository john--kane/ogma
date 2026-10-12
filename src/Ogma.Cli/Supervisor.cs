using System.Buffers.Binary;
using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Ogma;
internal static class Supervisor {
    public static async Task<Response> ExecuteAsync(Request request, CancellationToken cancellation) {
        var start = CreateStartInfo(request);
        using var process = new Process { StartInfo = start };
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        deadline.CancelAfter(TimeSpan.FromMilliseconds(request.Limits.TimeoutMs));
        var token = deadline.Token;
        if (!process.Start()) throw new CliFailure("OGMA_WORKER_CRASH", "Worker could not start");
        var stderr = DrainStderr(process.StandardError, token);
        var exchange = Exchange(process, request, token);
        var monitor = Monitor(process, request.Limits, token, request.Permissions.Model || request.Sandbox.Length != 0 || request.Command is "fmt" or "lint" or "bundle" or "check" or "install");
        try {
            var finished = await Task.WhenAny(exchange, monitor);
            await finished;
            var response = await exchange;
            await monitor;
            var details = await stderr;
            if (process.ExitCode != 0) throw new CliFailure("OGMA_WORKER_CRASH", $"Worker exited {process.ExitCode}: {details}");
            return ResponseValidation.Validate(response, request);
        } catch (OperationCanceledException) {
            throw new CliFailure(cancellation.IsCancellationRequested ? "OGMA_CANCELLED" : "OGMA_TIMEOUT",
                cancellation.IsCancellationRequested ? "Execution cancelled; no replay" : "Execution deadline exceeded; no replay");
        } catch (EndOfStreamException) {
            // A native crash commonly ends stdout mid-frame. Keep this distinct
            // from valid guest errors returned over IPC.
            throw new CliFailure("OGMA_WORKER_CRASH", "Worker closed the protocol stream; no replay");
        } catch (IOException error) {
            // A worker may exit before consuming the request, producing a
            // broken pipe on stdin rather than EOF on stdout.
            throw new CliFailure("OGMA_WORKER_CRASH", $"Worker protocol I/O failed: {error.Message}; no replay");
        } finally {
            try { if (!process.HasExited) process.Kill(entireProcessTree: true); } catch (InvalidOperationException) { }
            // Always reap the child. Kill is synchronous; waiting also closes pipes.
            await process.WaitForExitAsync(CancellationToken.None);
            deadline.Cancel();
            try { await Task.WhenAll(exchange, monitor, stderr); } catch (Exception) { /* original failure is preserved */ }
        }
    }
    internal static ProcessStartInfo CreateStartInfo(Request request) {
        var worker = Path.Combine(AppContext.BaseDirectory, OperatingSystem.IsWindows() ? "ogma-worker.exe" : "ogma-worker");
        if (!File.Exists(worker)) throw new CliFailure("OGMA_WORKER_MISSING", $"Worker missing: {worker}. Run tools/build.py.");
        var start = new ProcessStartInfo(worker) {
            UseShellExecute = false, RedirectStandardInput = true, RedirectStandardOutput = true,
            RedirectStandardError = true, CreateNoWindow = true,
        };
        // The worker gets only explicitly granted environment values. Engine and
        // loader control variables (LD_PRELOAD, JSC_*, etc.) are never inherited.
        start.Environment.Clear();
        foreach (var name in request.Permissions.Env) {
            if (name.StartsWith("JSC_", StringComparison.Ordinal) || name.StartsWith("DYLD_", StringComparison.Ordinal)
                || name.StartsWith("LD_", StringComparison.Ordinal))
                throw new CliFailure("OGMA_INVALID_ARGUMENT", $"Reserved worker environment variable: {name}");
            if (Environment.GetEnvironmentVariable(name) is { } value) start.Environment[name] = value;
        }
        if (OperatingSystem.IsWindows() && Environment.GetEnvironmentVariable("SystemRoot") is { } systemRoot) start.Environment["SystemRoot"] = systemRoot;
        SandboxLauncher.Configure(start, request, worker);
        return start;
    }
    private static async Task<Response> Exchange(Process process, Request request, CancellationToken token) {
        var bytes = JsonSerializer.SerializeToUtf8Bytes(request, ProtocolJson.Default.Request);
        if (bytes.Length > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "Request exceeds frame limit");
        byte[] header = new byte[4]; BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)bytes.Length);
        var input = process.StandardInput.BaseStream;
        await input.WriteAsync(header, token); await input.WriteAsync(bytes, token); await input.FlushAsync(token);
        process.StandardInput.Close();
        var output = process.StandardOutput.BaseStream;
        await output.ReadExactlyAsync(header, token);
        var size = BinaryPrimitives.ReadUInt32LittleEndian(header);
        if (size == 0 || size > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "Invalid response frame length");
        var payload = new byte[size]; await output.ReadExactlyAsync(payload, token);
        byte[] extra = new byte[1];
        if (await output.ReadAsync(extra, token) != 0) throw new CliFailure("OGMA_PROTOCOL", "Unexpected output after response frame");
        await process.WaitForExitAsync(token);
        try { return JsonSerializer.Deserialize(payload, ProtocolJson.Default.Response) ?? throw new CliFailure("OGMA_PROTOCOL", "Null worker response"); }
        catch (JsonException e) { throw new CliFailure("OGMA_PROTOCOL", e.Message); }
    }
    internal static async Task Monitor(Process process, Limits limits, CancellationToken token, bool descendants = false) {
        var exited = process.WaitForExitAsync(token);
        while (!exited.IsCompleted) {
            token.ThrowIfCancellationRequested();
            process.Refresh();
            try {
                if (await ResidentBytes(process, descendants, token) > (long)limits.MaxMemoryMb * 1024 * 1024)
                    throw new CliFailure("OGMA_RESOURCE_LIMIT", "Worker resident memory limit exceeded; no replay");
            } catch (InvalidOperationException) when (process.HasExited) { break; }
            // Process exit wakes the monitor immediately. A short-lived worker
            // should not pay for the rest of the memory sampling interval.
            await Task.WhenAny(exited, Task.Delay(descendants && OperatingSystem.IsMacOS() ? 100 : 10, token));
        }
        await exited;
    }
    private static async Task<long> ResidentBytes(Process process, bool descendants, CancellationToken token) {
        if (!descendants) return process.WorkingSet64;
        if (OperatingSystem.IsLinux()) {
            long total = 0;
            var todo = new Queue<int>(); todo.Enqueue(process.Id);
            var seen = new HashSet<int>();
            while (todo.Count > 0) {
                token.ThrowIfCancellationRequested();
                int pid = todo.Dequeue();
                if (!seen.Add(pid)) continue;
                if (seen.Count > 1024) throw new CliFailure("OGMA_RESOURCE_LIMIT", "Worker descendant process limit exceeded; no replay");
                try {
                    var resident = File.ReadAllText($"/proc/{pid}/statm").Split(' ', StringSplitOptions.RemoveEmptyEntries);
                    total = checked(total + long.Parse(resident[1], System.Globalization.CultureInfo.InvariantCulture) * Environment.SystemPageSize);
                    // A child can be forked by any thread, including a Node
                    // helper thread. Main-thread children alone miss that RSS.
                    foreach (var task in Directory.EnumerateDirectories($"/proc/{pid}/task")) {
                        token.ThrowIfCancellationRequested();
                        string children;
                        try { children = File.ReadAllText(task + "/children"); }
                        catch (Exception e) when (e is FileNotFoundException or DirectoryNotFoundException) { continue; }
                        foreach (var child in children.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
                            if (int.TryParse(child, out var childId)) todo.Enqueue(childId);
                    }
                } catch (Exception e) when (e is FileNotFoundException or DirectoryNotFoundException) { /* exited between samples */ }
            }
            return total;
        }
        if (OperatingSystem.IsMacOS()) {
            var start = new ProcessStartInfo("/bin/ps") { UseShellExecute = false,
                RedirectStandardOutput = true, RedirectStandardError = true };
            start.ArgumentList.Add("-axo"); start.ArgumentList.Add("pid=,ppid=,rss="); start.Environment.Clear();
            using var ps = Process.Start(start) ?? throw new CliFailure("OGMA_RESOURCE_LIMIT", "Cannot inspect worker descendants");
            using var probe = CancellationTokenSource.CreateLinkedTokenSource(token); probe.CancelAfter(1000);
            string rows;
            try {
                var stdout = ps.StandardOutput.ReadToEndAsync(probe.Token);
                var stderr = ps.StandardError.ReadToEndAsync(probe.Token);
                await ps.WaitForExitAsync(probe.Token); rows = await stdout; await stderr;
                if (ps.ExitCode != 0) throw new CliFailure("OGMA_RESOURCE_LIMIT", "Cannot inspect worker descendants");
            } finally { try { if (!ps.HasExited) ps.Kill(); } catch (InvalidOperationException) { } }
            var processes = new Dictionary<int, (int Parent, long Resident)>();
            foreach (var row in rows.Split('\n', StringSplitOptions.RemoveEmptyEntries)) {
                var values = row.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
                if (values.Length == 3 && int.TryParse(values[0], out int pid) && int.TryParse(values[1], out int parent)
                    && long.TryParse(values[2], out long rss)) processes[pid] = (parent, rss * 1024);
            }
            var seen = new HashSet<int> { process.Id }; long total = process.WorkingSet64;
            bool changed;
            do {
                changed = false;
                foreach (var item in processes) if (!seen.Contains(item.Key) && seen.Contains(item.Value.Parent)) {
                    seen.Add(item.Key); total = checked(total + item.Value.Resident); changed = true;
                }
            } while (changed);
            return total;
        }
        return process.WorkingSet64;
    }
    internal static async Task<string> DrainStderr(StreamReader reader, CancellationToken token) {
        char[] buffer = new char[2048]; var result = new StringBuilder(); int count;
        while ((count = await reader.ReadAsync(buffer.AsMemory(), token)) > 0) {
            if (result.Length < 16384) result.Append(buffer, 0, Math.Min(count, 16384 - result.Length));
        }
        return result.ToString();
    }
}
