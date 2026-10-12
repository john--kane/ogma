using System.Buffers.Binary;
using System.Diagnostics;
using System.Text.Json;

namespace Ogma;

// One explicit session owns one worker. A failed exchange permanently ends the
// session: restarting would lose or replay guest state and side effects.
internal sealed class InteractiveSupervisor : IAsyncDisposable {
    private readonly Process process;
    private readonly CancellationTokenSource lifetime = new();
    private readonly Task<string> stderr;
    private bool failed;
    private readonly Task? sessionMonitor;
    public InteractiveSupervisor(Request request) {
        process = new Process { StartInfo = Supervisor.CreateStartInfo(request) };
        if (!process.Start()) throw new CliFailure("OGMA_WORKER_CRASH", "REPL worker could not start");
        stderr = Supervisor.DrainStderr(process.StandardError, lifetime.Token);
        if (request.Permissions.Model) sessionMonitor = MonitorSession(request.Limits);
    }
    private async Task MonitorSession(Limits limits) {
        try {
            await Supervisor.Monitor(process, limits, lifetime.Token, descendants: true);
            throw new CliFailure("OGMA_WORKER_CRASH", "Model worker exited; session ended without replay");
        } catch {
            failed = true;
            Stop();
            throw;
        }
    }
    // Waiting for input must retain the same resident-memory and worker-liveness
    // supervision as an active inference request.
    public async Task<T> WaitWhileIdle<T>(Func<CancellationToken, Task<T>> operation, CancellationToken cancellation) {
        using var wait = CancellationTokenSource.CreateLinkedTokenSource(cancellation, lifetime.Token);
        // Console readers may perform a synchronous read even via ReadLineAsync.
        // Keep that read off the supervision thread and never wait for it after
        // cancellation/session failure. No subsequent session is started.
        var pending = Task.Run(() => operation(wait.Token), wait.Token);
        try {
            if (sessionMonitor is not null && await Task.WhenAny(pending, sessionMonitor).WaitAsync(wait.Token) == sessionMonitor)
                await sessionMonitor;
            return await pending.WaitAsync(wait.Token);
        } finally {
            wait.Cancel();
        }
    }
    public async Task<Response> ExecuteAsync(Request request, CancellationToken cancellation) {
        if (sessionMonitor?.IsCompleted == true) await sessionMonitor;
        if (failed) throw new CliFailure("OGMA_WORKER_CRASH", "REPL session ended; no replay");
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellation, lifetime.Token);
        deadline.CancelAfter(TimeSpan.FromMilliseconds(request.Limits.TimeoutMs));
        var token = deadline.Token;
        var exchange = Exchange(request, token);
        var monitor = sessionMonitor is null
            ? Supervisor.Monitor(process, request.Limits, token, request.Sandbox.Length != 0)
            : Task.Delay(Timeout.Infinite, token);
        try {
            var finished = sessionMonitor is null ? await Task.WhenAny(exchange, monitor) : await Task.WhenAny(exchange, monitor, sessionMonitor);
            await finished;
            // A deliberate session-ending diagnostic may be framed immediately
            // before process exit. Read it even when the exit monitor wins.
            var response = await exchange;
            if (process.HasExited && process.ExitCode != 0)
                throw new CliFailure("OGMA_WORKER_CRASH", "REPL worker exited abnormally; no replay");
            return ResponseValidation.Validate(response, request);
        } catch (OperationCanceledException) {
            failed = true;
            throw new CliFailure(cancellation.IsCancellationRequested ? "OGMA_CANCELLED" : "OGMA_TIMEOUT",
                "REPL evaluation cancelled or timed out; session ended without replay");
        } catch (Exception e) when (e is EndOfStreamException or IOException) {
            failed = true;
            throw new CliFailure("OGMA_WORKER_CRASH", "REPL worker closed the protocol stream; no replay");
        } catch { failed = true; throw; }
        finally {
            deadline.Cancel();
            if (failed) Stop();
            try { await Task.WhenAll(exchange, monitor); } catch { }
        }
    }
    private async Task<Response> Exchange(Request request, CancellationToken token) {
        var payload = JsonSerializer.SerializeToUtf8Bytes(request, ProtocolJson.Default.Request);
        if (payload.Length > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "Request exceeds frame limit");
        var header = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)payload.Length);
        var input = process.StandardInput.BaseStream;
        await input.WriteAsync(header, token); await input.WriteAsync(payload, token); await input.FlushAsync(token);
        var output = process.StandardOutput.BaseStream;
        await output.ReadExactlyAsync(header, token);
        var size = BinaryPrimitives.ReadUInt32LittleEndian(header);
        if (size == 0 || size > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "Invalid response frame length");
        payload = new byte[size]; await output.ReadExactlyAsync(payload, token);
        try { return JsonSerializer.Deserialize(payload, ProtocolJson.Default.Response)
            ?? throw new CliFailure("OGMA_PROTOCOL", "Null worker response"); }
        catch (JsonException e) { throw new CliFailure("OGMA_PROTOCOL", e.Message); }
    }
    private void Stop() {
        try { if (!process.HasExited) process.Kill(entireProcessTree: true); } catch (InvalidOperationException) { }
    }
    public async ValueTask DisposeAsync() {
        lifetime.Cancel();
        Stop();
        await process.WaitForExitAsync();
        if (sessionMonitor is not null) { try { await sessionMonitor; } catch { } }
        try { await stderr; } catch (OperationCanceledException) { }
        lifetime.Dispose(); process.Dispose();
    }
}
