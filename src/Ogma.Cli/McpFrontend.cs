using System.Collections.Concurrent;
using System.Globalization;
using System.Text.Json;

namespace Ogma;

// MCP stdio is newline-delimited JSON, distinct from LSP Content-Length framing.
// The frontend owns protocol/lifecycle only. Every tool uses the existing worker
// supervisor, fixed CLI policy and resource controls, without automatic retries.
internal sealed class McpFrontend(Options options) {
    private readonly Stream input = Console.OpenStandardInput();
    // Unix Console streams silently ignore EPIPE. A FileStream preserves it so
    // a disconnected client cancels work instead of leaving a reader running.
    private readonly Stream output = OperatingSystem.IsWindows() ? Console.OpenStandardOutput()
        : new FileStream(new Microsoft.Win32.SafeHandles.SafeFileHandle((nint)1, ownsHandle: false), FileAccess.Write);
    private readonly SemaphoreSlim writes = new(1);
    private readonly ConcurrentDictionary<string, CancellationTokenSource> active = new();
    private readonly CancellationTokenSource lifetime = new();
    private readonly List<Task> pending = [];
    private bool initialized, ready;
    private long sequence;

    public static Task<int> Run(Options options) => new McpFrontend(options).Run();

    private async Task<int> Run() {
        ConsoleCancelEventHandler handler = (_, e) => { e.Cancel = true; lifetime.Cancel(); CancelAll(); };
        Console.CancelKeyPress += handler;
        if (options.Explain) Terminal.WriteLine("MCP stdio: fixed CLI grants; fresh isolated Rust/JSC worker per discovery/call, supervised cancellation and limits; no replay. No listening socket.", error: true);
        try {
            var lines = new LineReader(input);
            while (true) {
                var bytes = await lines.Read(lifetime.Token).WaitAsync(lifetime.Token);
                if (bytes is null) break;
                if (bytes.Length == 0) continue;
                JsonDocument message;
                try { message = JsonDocument.Parse(bytes, new JsonDocumentOptions { MaxDepth = 64 }); }
                catch (JsonException) { await Error(null, -32700, "Parse error"); continue; }
                using (message) await Accept(message.RootElement);
                pending.RemoveAll(task => task.IsCompletedSuccessfully);
            }
            return 0;
        } catch (OperationCanceledException) when (lifetime.IsCancellationRequested) { return 1; }
        catch (CliFailure error) { await Error(null, -32700, error.Message, error.Code); return 1; }
        catch (IOException error) { Terminal.WriteLine($"OGMA_IO: {error.Message}", error: true); return 1; }
        finally {
            lifetime.Cancel(); CancelAll();
            try { await Task.WhenAll(pending); }
            catch (OperationCanceledException) { }
            catch (IOException error) { Terminal.WriteLine($"OGMA_IO: {error.Message}", error: true); }
            Console.CancelKeyPress -= handler;
            lifetime.Dispose(); writes.Dispose();
        }
    }

    private async Task Accept(JsonElement root) {
        JsonElement? id = null;
        if (root.ValueKind == JsonValueKind.Object && root.TryGetProperty("id", out var value)) {
            if (value.ValueKind == JsonValueKind.String || value.ValueKind == JsonValueKind.Number && value.TryGetInt64(out _)) id = value.Clone();
            else { await Error(null, -32600, "Request id must be a string or integer"); return; }
        }
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("jsonrpc", out var version) || version.ValueKind != JsonValueKind.String || version.GetString() != "2.0"
            || !root.TryGetProperty("method", out var method) || method.ValueKind != JsonValueKind.String) {
            await Error(id, -32600, "Invalid Request"); return;
        }
        var parameters = root.TryGetProperty("params", out var p) ? p : default;
        if (parameters.ValueKind is not (JsonValueKind.Object or JsonValueKind.Undefined)) {
            if (id.HasValue) await Error(id, -32602, "Parameters must be an object"); return;
        }
        try {
            switch (method.GetString()) {
                case "initialize":
                    if (!id.HasValue) return;
                    if (initialized || active.Count != 0) { await Error(id, -32600, "Already initialized"); return; }
                    var requested = Text(parameters, "protocolVersion");
                    if (!parameters.TryGetProperty("capabilities", out var capabilities) || capabilities.ValueKind != JsonValueKind.Object
                        || !parameters.TryGetProperty("clientInfo", out var client) || client.ValueKind != JsonValueKind.Object)
                        throw new ArgumentException("initialize requires capabilities and clientInfo objects");
                    Text(client, "name"); Text(client, "version");
                    var negotiated = Contract.McpProtocolVersions.Contains(requested) ? requested : Contract.McpProtocolVersions[0];
                    initialized = true;
                    await Result(id, w => {
                        w.WriteStartObject(); w.WriteString("protocolVersion", negotiated);
                        w.WriteStartObject("capabilities"); w.WriteStartObject("tools"); w.WriteEndObject(); w.WriteEndObject();
                        w.WriteStartObject("serverInfo"); w.WriteString("name", "ogma"); w.WriteString("version", Program.Version); w.WriteEndObject(); w.WriteEndObject();
                    }); return;
                case "notifications/initialized":
                    if (initialized && !id.HasValue) ready = true;
                    return;
                case "notifications/cancelled":
                    if (id.HasValue) { await Error(id, -32600, "Cancellation is a notification"); return; }
                    if (parameters.TryGetProperty("requestId", out var cancel) && active.TryGetValue(Key(cancel), out var cts)) {
                        try { cts.Cancel(); } catch (ObjectDisposedException) { }
                    }
                    return;
                case "ping":
                    if (id.HasValue) await Result(id, w => { w.WriteStartObject(); w.WriteEndObject(); });
                    return;
                case "tools/list":
                case "tools/call":
                    if (!id.HasValue) return;
                    if (!ready) { await Error(id, -32002, "Server is not initialized"); return; }
                    var list = method.GetString() == "tools/list";
                    if (parameters.ValueKind == JsonValueKind.Object) foreach (var property in parameters.EnumerateObject())
                        if (property.Name != "_meta" && (list || property.Name is not ("name" or "arguments"))) throw new ArgumentException($"Unsupported parameter: {property.Name}");
                    var invocation = new McpInvocation();
                    if (!list) {
                        var name = Text(parameters, "name");
                        using var empty = JsonDocument.Parse("{}");
                        var arguments = parameters.TryGetProperty("arguments", out var a) ? a : empty.RootElement;
                        if (arguments.ValueKind != JsonValueKind.Object) throw new ArgumentException("Tool arguments must be an object");
                        invocation = new() { Name = name, Arguments = arguments.Clone() };
                    }
                    var identity = id.Value.Clone(); var key = Key(identity);
                    var cancellation = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
                    if (active.Count >= 4 || !active.TryAdd(key, cancellation)) {
                        cancellation.Dispose(); await Error(id, -32600, "Duplicate id or four active tool requests already admitted"); return;
                    }
                    var request = options.Request() with { Id = $"mcp-{Interlocked.Increment(ref sequence)}", Command = list ? "mcp-list" : "mcp-call", Mcp = invocation };
                    pending.Add(Execute(identity, key, request, cancellation)); return;
                default:
                    if (id.HasValue) await Error(id, -32601, "Method not found"); return;
            }
        } catch (Exception e) when (e is ArgumentException or InvalidOperationException or KeyNotFoundException or FormatException or OverflowException) {
            if (id.HasValue) await Error(id, -32602, e.Message);
        }
    }

    private async Task Execute(JsonElement id, string key, Request request, CancellationTokenSource cancellation) {
        try {
            var response = await Supervisor.ExecuteAsync(request, cancellation.Token);
            foreach (var item in response.Output) Terminal.WriteLine(item.Text, error: true);
            foreach (var trace in response.Trace) Terminal.Trace(trace);
            if (response.Status != "ok") {
                var text = string.Join("\n", response.Diagnostics.Select(d => $"{d.Code}: {d.Message}"));
                if (request.Command == "mcp-list" || response.Diagnostics.Any(d => d.Code == "OGMA_INVALID_ARGUMENT"))
                    await Error(id, response.Diagnostics.Any(d => d.Code == "OGMA_INVALID_ARGUMENT") ? -32602 : -32603, text);
                else await ToolResult(id, text, true);
                return;
            }
            if (request.Command == "mcp-call") {
                if (response.Result is not { ValueKind: JsonValueKind.String } text) throw new CliFailure("OGMA_PROTOCOL", "Worker returned invalid MCP tool text");
                await ToolResult(id, text.GetString()!, false);
            } else {
                if (response.Result is not { ValueKind: JsonValueKind.Array } tools || tools.GetArrayLength() > Contract.MaxMcpTools) throw new CliFailure("OGMA_PROTOCOL", "Worker returned invalid MCP tools");
                var names = new HashSet<string>(StringComparer.Ordinal);
                foreach (var item in tools.EnumerateArray()) {
                    var tool = JsonSerializer.Deserialize(item, ProtocolJson.Default.McpTool);
                    if (tool is null || tool.Name is null || tool.Name.Length is < 1 or > 128 || !names.Add(tool.Name)
                        || !tool.Name.All(c => char.IsAsciiLetterOrDigit(c) || "_.-".Contains(c))
                        || tool.Description is null || tool.Description.Length > 16384 || tool.InputSchema is not { ValueKind: JsonValueKind.Object })
                        throw new CliFailure("OGMA_PROTOCOL", "Worker returned invalid MCP tool metadata");
                }
                await Result(id, w => { w.WriteStartObject(); w.WritePropertyName("tools"); tools.WriteTo(w); w.WriteEndObject(); });
            }
        } catch (CliFailure e) { await Error(id, e.Code == "OGMA_CANCELLED" ? -32800 : -32000, e.Message, e.Code); }
        catch (JsonException e) { await Error(id, -32603, e.Message, "OGMA_PROTOCOL"); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception) { await Error(id, -32603, e.Message, "OGMA_IO"); }
        finally { active.TryRemove(key, out _); cancellation.Dispose(); }
    }

    private Task ToolResult(JsonElement id, string text, bool error) => Result(id, w => {
        w.WriteStartObject(); w.WriteStartArray("content"); w.WriteStartObject(); w.WriteString("type", "text"); w.WriteString("text", text); w.WriteEndObject(); w.WriteEndArray(); w.WriteBoolean("isError", error); w.WriteEndObject();
    });
    private Task Result(JsonElement? id, Action<Utf8JsonWriter> result) => Send(w => {
        w.WriteStartObject(); w.WriteString("jsonrpc", "2.0"); WriteId(w, id); w.WritePropertyName("result"); result(w); w.WriteEndObject();
    });
    private Task Error(JsonElement? id, int code, string message, string? data = null) => Send(w => {
        w.WriteStartObject(); w.WriteString("jsonrpc", "2.0"); WriteId(w, id); w.WriteStartObject("error"); w.WriteNumber("code", code); w.WriteString("message", message); if (data is not null) w.WriteString("data", data); w.WriteEndObject(); w.WriteEndObject();
    });
    private static void WriteId(Utf8JsonWriter writer, JsonElement? id) { writer.WritePropertyName("id"); if (id.HasValue) id.Value.WriteTo(writer); else writer.WriteNullValue(); }
    private async Task Send(Action<Utf8JsonWriter> write) {
        if (lifetime.IsCancellationRequested) return;
        using var buffer = new MemoryStream(); using (var writer = new Utf8JsonWriter(buffer)) write(writer);
        if (buffer.Length > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "MCP response exceeds frame limit");
        await writes.WaitAsync(lifetime.Token);
        try { await output.WriteAsync(buffer.ToArray(), lifetime.Token); await output.WriteAsync(new byte[] { 10 }, lifetime.Token); await output.FlushAsync(lifetime.Token); }
        catch (IOException) { lifetime.Cancel(); CancelAll(); throw; }
        finally { writes.Release(); }
    }
    private void CancelAll() { foreach (var cts in active.Values) try { cts.Cancel(); } catch (ObjectDisposedException) { } }
    private static string Key(JsonElement id) => id.ValueKind switch {
        JsonValueKind.String => "string:" + id.GetString(),
        JsonValueKind.Number when id.TryGetDecimal(out var n) => "number:" + n.ToString("G29", CultureInfo.InvariantCulture),
        _ => "invalid",
    };
    private static string Text(JsonElement parameters, string name) {
        if (parameters.ValueKind != JsonValueKind.Object || !parameters.TryGetProperty(name, out var value) || value.ValueKind != JsonValueKind.String || string.IsNullOrEmpty(value.GetString())) throw new ArgumentException($"{name} must be a nonempty string");
        return value.GetString()!;
    }

    private sealed class LineReader(Stream stream) {
        private readonly byte[] buffer = new byte[8192];
        private int offset, available;
        public async Task<byte[]?> Read(CancellationToken token) {
            using var message = new MemoryStream();
            while (true) {
                if (offset == available) {
                    available = await stream.ReadAsync(buffer, token); offset = 0;
                    if (available == 0) {
                        if (message.Length == 0) return null;
                        throw new CliFailure("OGMA_PROTOCOL", "Truncated MCP line: a newline is required");
                    }
                }
                var newline = Array.IndexOf(buffer, (byte)10, offset, available - offset);
                var end = newline < 0 ? available : newline;
                if (message.Length + end - offset > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "MCP line exceeds 8 MiB");
                message.Write(buffer, offset, end - offset); offset = end;
                if (newline >= 0) { offset++; return message.ToArray(); }
            }
        }
    }
}
