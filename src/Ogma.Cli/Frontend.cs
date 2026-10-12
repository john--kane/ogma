using System.Collections.Concurrent;
using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Ogma;
internal static class Frontend {
    public static async Task<int> RunRepl(Options options) {
        var request = options.Request() with { Command = "repl" };
        if (options.Trusted) throw new CliFailure("OGMA_INVALID_ARGUMENT", "REPL requires an isolated worker");
        if (options.DryRun) { Console.WriteLine(JsonSerializer.Serialize(request, ProtocolJson.Default.Request)); return 0; }
        if (!Console.IsInputRedirected) Terminal.WriteLine("Ogma REPL preview. Enter JS/TS; .exit ends the session. Bindings persist as mutable globals.", error: true);
        await using var session = new InteractiveSupervisor(request);
        using var cancellation = new CancellationTokenSource();
        ConsoleCancelEventHandler handler = (_, e) => { e.Cancel = true; cancellation.Cancel(); };
        Console.CancelKeyPress += handler;
        try {
            int sequence = 0;
            while (true) {
                if (!Console.IsInputRedirected) Terminal.Prompt("> ");
                var line = await session.WaitWhileIdle(token => Console.In.ReadLineAsync(token).AsTask(), cancellation.Token);
                if (line is null || line.Trim() == ".exit") return 0;
                if (string.IsNullOrWhiteSpace(line)) continue;
                var response = await session.ExecuteAsync(request with { Id = $"repl-{++sequence}", Source = line }, cancellation.Token);
                Program.Render(response, options.Json);
                if (response.Diagnostics.Any(d => d.Code is "OGMA_TIMEOUT" or "OGMA_RESOURCE_LIMIT" or "OGMA_PANIC")) return 1;
            }
        } catch (OperationCanceledException) { throw new CliFailure("OGMA_CANCELLED", "REPL cancelled; no replay"); }
        finally { Console.CancelKeyPress -= handler; }
    }
    public static async Task<int> RunRpc(Options options, bool lsp) {
        if (options.Trusted || options.DryRun) throw new CliFailure("OGMA_INVALID_ARGUMENT", "RPC/LSP requires isolated execution and cannot use dry-run");
        return await new RpcSession(options, lsp).Run();
    }
    private sealed record Document(string Uri, string Entry, string Text, int Version);
    private sealed class RpcSession(Options options, bool lsp) {
        private readonly Stream input = Console.OpenStandardInput();
        private readonly Stream output = Console.OpenStandardOutput();
        private readonly SemaphoreSlim writes = new(1);
        private readonly SemaphoreSlim checkerSlots = new(4);
        private readonly ConcurrentDictionary<string, CancellationTokenSource> active = new();
        private readonly Dictionary<string, Document> documents = new(StringComparer.Ordinal);
        private readonly ConcurrentDictionary<string, CancellationTokenSource> checks = new(StringComparer.Ordinal);
        private readonly CancellationTokenSource lifetime = new();
        private readonly List<Task> pending = [];
        private bool initialized, shutdown, exited;
        private long sequence;
        public async Task<int> Run() {
            ConsoleCancelEventHandler handler = (_, e) => { e.Cancel = true; lifetime.Cancel(); CancelAll(); };
            Console.CancelKeyPress += handler;
            try {
                while (!exited) {
                    var bytes = await ReadMessage(input, lifetime.Token).WaitAsync(lifetime.Token);
                    if (bytes is null) break;
                    JsonDocument parsed;
                    try { parsed = JsonDocument.Parse(bytes, new JsonDocumentOptions { MaxDepth = 64 }); }
                    catch (JsonException) { await Error(null, -32700, "Parse error"); continue; }
                    using (parsed) await Accept(parsed.RootElement);
                    pending.RemoveAll(t => t.IsCompleted);
                }
                await Task.WhenAll(pending);
                return lsp && exited && !shutdown ? 1 : 0;
            } catch (OperationCanceledException) {
                CancelAll();
                try { await Task.WhenAll(pending); } catch (OperationCanceledException) { }
                return 1;
            } catch (CliFailure e) {
                await Error(null, -32700, e.Message);
                CancelAll();
                await Task.WhenAll(pending);
                return 1;
            } finally {
                Console.CancelKeyPress -= handler;
                foreach (var cts in checks.Values) cts.Dispose();
                lifetime.Dispose(); writes.Dispose(); checkerSlots.Dispose();
            }
        }
        private async Task Accept(JsonElement root) {
            JsonElement? id = null;
            if (root.ValueKind == JsonValueKind.Object && root.TryGetProperty("id", out var identity)) {
                if (identity.ValueKind is JsonValueKind.String or JsonValueKind.Number or JsonValueKind.Null) id = identity.Clone();
                else { await Error(null, -32600, "Invalid request id"); return; }
            }
            if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("jsonrpc", out var version)
                || version.ValueKind != JsonValueKind.String || version.GetString() != "2.0"
                || !root.TryGetProperty("method", out var methodValue) || methodValue.ValueKind != JsonValueKind.String) {
                await Error(id, -32600, "Invalid Request"); return;
            }
            var method = methodValue.GetString()!;
            var parameters = root.TryGetProperty("params", out var p) ? p.Clone() : default;
            if (parameters.ValueKind is not (JsonValueKind.Undefined or JsonValueKind.Object)) {
                if (id.HasValue) await Error(id, -32602, "Parameters must be an object"); return;
            }
            try {
                if (method == "$/cancelRequest") {
                    if (parameters.TryGetProperty("id", out var cancel) && active.TryGetValue(Key(cancel), out var cts)) {
                        try { cts.Cancel(); } catch (ObjectDisposedException) { }
                    }
                    return;
                }
                if (lsp) {
                    if (method == "exit") {
                        exited = true;
                        CancelAll();
                        return;
                    }
                    if (method == "initialize" && !initialized && !shutdown) {
                        initialized = true;
                        if (id.HasValue) await Result(id, w => {
                            w.WriteStartObject(); w.WriteStartObject("capabilities");
                            w.WriteStartObject("textDocumentSync"); w.WriteBoolean("openClose", true); w.WriteNumber("change", 1);
                            w.WriteBoolean("save", true); w.WriteEndObject(); w.WriteEndObject();
                            w.WriteStartObject("serverInfo"); w.WriteString("name", "Ogma LSP preview"); w.WriteString("version", "0.1.0-preview");
                            w.WriteEndObject(); w.WriteEndObject();
                        });
                        return;
                    }
                    if (!initialized) { if (id.HasValue) await Error(id, -32002, "Server not initialized"); return; }
                    if (method == "shutdown") {
                        shutdown = true;
                        CancelAll();
                        if (id.HasValue) await Result(id, w => w.WriteNullValue()); return;
                    }
                    if (shutdown) { if (id.HasValue) await Error(id, -32600, "Server is shut down"); return; }
                    if (method == "initialized") return;
                    if (method.StartsWith("textDocument/", StringComparison.Ordinal)) {
                        await DocumentMessage(method, parameters, id); return;
                    }
                }
                if (method == "capabilities" && !lsp) {
                    if (id.HasValue) await Result(id, w => {
                        w.WriteStartObject(); w.WriteString("engine", "JavaScriptCore"); w.WriteNumber("protocolVersion", Contract.ProtocolVersion);
                        w.WriteStartArray("methods"); foreach (var name in new[] { "run", "eval", "check", "capabilities", "$/cancelRequest" }) w.WriteStringValue(name);
                        w.WriteEndArray(); w.WriteString("permissions", "fixed by server CLI flags"); w.WriteEndObject();
                    }); return;
                }
                if (method is not ("run" or "eval" or "check") || lsp) {
                    if (id.HasValue) await Error(id, -32601, "Method not found"); return;
                }
                if (!id.HasValue) return; // Execution always has a response/cancellation identity.
                var source = StringParameter(parameters, "source", method == "eval");
                var entry = StringParameter(parameters, "entry", method != "eval");
                var request = options.Request() with { Id = $"rpc-{Interlocked.Increment(ref sequence)}", Command = method,
                    Source = method == "check" && parameters.TryGetProperty("source", out _) && source.Length == 0 ? "\n" : source, Entry = entry.Length == 0 ? "" : Path.GetFullPath(entry) };
                var key = Key(id.Value);
                var token = new CancellationTokenSource();
                if (active.Count >= 32 || !active.TryAdd(key, token)) {
                    token.Dispose(); await Error(id, -32600, "Duplicate request id or too many active requests"); return;
                }
                pending.Add(Execute(id.Value, key, request, token));
            } catch (Exception e) when (e is ArgumentException or InvalidOperationException or UriFormatException or KeyNotFoundException or FormatException or OverflowException) {
                if (id.HasValue) await Error(id, -32602, e.Message);
            }
        }
        private async Task Execute(JsonElement id, string key, Request request, CancellationTokenSource cancellation) {
            try {
                var response = await Supervisor.ExecuteAsync(request, cancellation.Token);
                await Result(id, w => JsonSerializer.Serialize(w, response, ProtocolJson.Default.Response));
            } catch (CliFailure e) { await Error(id, e.Code == "OGMA_CANCELLED" ? -32800 : -32000, e.Message, e.Code); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception) {
                await Error(id, -32603, e.Message);
            } finally {
                active.TryRemove(key, out _); cancellation.Dispose();
            }
        }
        private async Task DocumentMessage(string method, JsonElement parameters, JsonElement? id) {
            if (id.HasValue) { await Error(id, -32601, "Unsupported document request"); return; }
            var doc = parameters.GetProperty("textDocument");
            var uri = StringParameter(doc, "uri", true);
            var parsed = new Uri(uri, UriKind.Absolute);
            if (!parsed.IsFile || parsed.Host is not ("" or "localhost")) throw new ArgumentException("Only local file documents are supported");
            if (method == "textDocument/didClose") {
                documents.Remove(uri);
                if (checks.TryRemove(uri, out var old)) { old.Cancel(); old.Dispose(); }
                await Diagnostics(uri, null, []); return;
            }
            Document current;
            if (method == "textDocument/didOpen") {
                current = new(uri, Path.GetFullPath(parsed.LocalPath), StringParameter(doc, "text", true), doc.GetProperty("version").GetInt32());
            } else {
                if (!documents.TryGetValue(uri, out current!)) return;
                if (method == "textDocument/didChange") {
                    var changes = parameters.GetProperty("contentChanges");
                    if (changes.ValueKind != JsonValueKind.Array || changes.GetArrayLength() != 1
                        || changes[0].TryGetProperty("range", out _)) throw new ArgumentException("Only full document changes are supported");
                    var version = doc.GetProperty("version").GetInt32();
                    if (version <= current.Version) return;
                    current = current with { Text = StringParameter(changes[0], "text", true), Version = version };
                } else if (method == "textDocument/didSave") {
                    if (parameters.TryGetProperty("text", out var text)) {
                        if (text.ValueKind != JsonValueKind.String) throw new ArgumentException("Invalid saved text");
                        current = current with { Text = text.GetString()! };
                    }
                } else return;
            }
            if (documents.Count >= 128 && !documents.ContainsKey(uri)) throw new ArgumentException("Too many open documents");
            if (Encoding.UTF8.GetByteCount(current.Text) > 1_048_576) throw new ArgumentException("Document exceeds 1 MiB limit");
            documents[uri] = current;
            if (checks.TryRemove(uri, out var previous)) { previous.Cancel(); previous.Dispose(); }
            var cancellation = new CancellationTokenSource(); checks[uri] = cancellation;
            pending.Add(CheckDocument(current, cancellation));
        }
        private async Task CheckDocument(Document document, CancellationTokenSource cancellation) {
            var token = cancellation.Token;
            Response response;
            try {
                var request = options.Request() with { Command = "check", Entry = document.Entry, Source = document.Text.Length == 0 ? "\n" : document.Text,
                    Id = $"lsp-{Interlocked.Increment(ref sequence)}" };
                await checkerSlots.WaitAsync(token);
                try { response = await Supervisor.ExecuteAsync(request, token); }
                finally { checkerSlots.Release(); }
            } catch (OperationCanceledException) when (token.IsCancellationRequested) { return; }
            catch (CliFailure e) {
                if (cancellation.IsCancellationRequested) return;
                response = new Response { Diagnostics = [new Diagnostic { Code = e.Code, Message = e.Message }] };
            } catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception) {
                response = new Response { Diagnostics = [new Diagnostic { Code = "OGMA_IO", Message = e.Message }] };
            }
            if (!cancellation.IsCancellationRequested) await Diagnostics(document.Uri, document.Version,
                response.Diagnostics.Where(d => d.Specifier.Length == 0 || SameFile(d.Specifier, document.Entry)).ToArray(), token);
        }
        private static bool SameFile(string specifier, string entry) {
            try { return Path.GetFullPath(specifier.StartsWith("file:", StringComparison.Ordinal) ? new Uri(specifier).LocalPath : specifier) == entry; }
            catch (Exception e) when (e is ArgumentException or UriFormatException) { return false; }
        }
        private Task Diagnostics(string uri, int? version, Diagnostic[] diagnostics, CancellationToken cancellation = default) => Send(w => {
            w.WriteStartObject(); w.WriteString("jsonrpc", "2.0"); w.WriteString("method", "textDocument/publishDiagnostics");
            w.WriteStartObject("params"); w.WriteString("uri", uri); if (version.HasValue) w.WriteNumber("version", version.Value);
            w.WriteStartArray("diagnostics");
            foreach (var d in diagnostics) {
                w.WriteStartObject(); w.WriteStartObject("range");
                foreach (var point in new[] { "start", "end" }) {
                    w.WriteStartObject(point); w.WriteNumber("line", Math.Max(0, (long)d.Line - 1));
                    w.WriteNumber("character", Math.Max(0, (long)d.Column - 1) + (point == "end" ? 1 : 0)); w.WriteEndObject();
                }
                w.WriteEndObject(); w.WriteNumber("severity", 1); w.WriteString("code", d.Code);
                w.WriteString("source", "ogma"); w.WriteString("message", d.Message); w.WriteEndObject();
            }
            w.WriteEndArray(); w.WriteEndObject(); w.WriteEndObject();
        }, cancellation);
        private Task Result(JsonElement? id, Action<Utf8JsonWriter> result) => Send(w => {
            w.WriteStartObject(); w.WriteString("jsonrpc", "2.0"); WriteId(w, id);
            w.WritePropertyName("result"); result(w); w.WriteEndObject();
        });
        private Task Error(JsonElement? id, int code, string message, string? data = null) => Send(w => {
            w.WriteStartObject(); w.WriteString("jsonrpc", "2.0"); WriteId(w, id);
            w.WriteStartObject("error"); w.WriteNumber("code", code); w.WriteString("message", message);
            if (data is not null) w.WriteString("data", data); w.WriteEndObject(); w.WriteEndObject();
        });
        private static void WriteId(Utf8JsonWriter w, JsonElement? id) {
            w.WritePropertyName("id"); if (id.HasValue) id.Value.WriteTo(w); else w.WriteNullValue();
        }
        private async Task Send(Action<Utf8JsonWriter> write, CancellationToken cancellation = default) {
            using var buffer = new MemoryStream(); using (var writer = new Utf8JsonWriter(buffer)) write(writer);
            if (buffer.Length > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "RPC response exceeds frame limit");
            var payload = buffer.ToArray(); var header = Encoding.ASCII.GetBytes($"Content-Length: {payload.Length}\r\n\r\n");
            await writes.WaitAsync();
            try { if (cancellation.IsCancellationRequested) return; await output.WriteAsync(header, lifetime.Token); await output.WriteAsync(payload, lifetime.Token); await output.FlushAsync(lifetime.Token); }
            finally { writes.Release(); }
        }
        private void CancelAll() {
            foreach (var cts in active.Values) { try { cts.Cancel(); } catch (ObjectDisposedException) { } }
            foreach (var cts in checks.Values) { try { cts.Cancel(); } catch (ObjectDisposedException) { } }
        }
        private static string Key(JsonElement id) => id.ValueKind switch {
            JsonValueKind.String => "string:" + id.GetString(),
            JsonValueKind.Number when id.TryGetDecimal(out var number) => "number:" + number.ToString("G29", CultureInfo.InvariantCulture),
            _ => id.ValueKind + ":" + id.GetRawText(),
        };
        private static string StringParameter(JsonElement parameters, string name, bool required) {
            if (parameters.ValueKind == JsonValueKind.Object && parameters.TryGetProperty(name, out var value)) {
                if (value.ValueKind != JsonValueKind.String) throw new ArgumentException($"{name} must be a string");
                return value.GetString()!;
            }
            if (required) throw new ArgumentException($"Missing {name}");
            return "";
        }
    }
    private static async Task<byte[]?> ReadMessage(Stream input, CancellationToken cancellation) {
        // Bounded byte-wise header parsing avoids StreamReader buffering bytes
        // belonging to the next UTF-8 payload. Payload length counts bytes.
        var header = new List<byte>(256); var one = new byte[1];
        while (true) {
            if (await input.ReadAsync(one, cancellation) == 0) {
                if (header.Count == 0) return null;
                throw new CliFailure("OGMA_PROTOCOL", "Truncated RPC header");
            }
            header.Add(one[0]);
            if (header.Count > 8192 || one[0] > 127) throw new CliFailure("OGMA_PROTOCOL", "Invalid or oversized RPC header");
            var n = header.Count;
            if (n >= 4 && header[n-4] == 13 && header[n-3] == 10 && header[n-2] == 13 && header[n-1] == 10) break;
        }
        int? size = null;
        foreach (var line in Encoding.ASCII.GetString(header.ToArray()).Split("\r\n", StringSplitOptions.RemoveEmptyEntries)) {
            var parts = line.Split(':', 2);
            if (parts.Length != 2) throw new CliFailure("OGMA_PROTOCOL", "Malformed RPC header");
            if (!parts[0].Equals("Content-Length", StringComparison.OrdinalIgnoreCase)) continue;
            if (size.HasValue || !int.TryParse(parts[1].Trim(), NumberStyles.None, CultureInfo.InvariantCulture, out int length)
                || length <= 0 || length > Contract.MaxFrameBytes) throw new CliFailure("OGMA_PROTOCOL", "Invalid Content-Length");
            size = length;
        }
        if (!size.HasValue) throw new CliFailure("OGMA_PROTOCOL", "Missing Content-Length");
        var payload = new byte[size.Value];
        try { await input.ReadExactlyAsync(payload, cancellation); }
        catch (EndOfStreamException) { throw new CliFailure("OGMA_PROTOCOL", "Truncated RPC payload"); }
        return payload;
    }
}
