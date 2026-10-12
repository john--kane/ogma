using System.Text;
using System.Text.Json;
namespace Ogma;
internal static class Program {
    internal const string Version = "0.1.0-preview.1";
    public static async Task<int> Main(string[] args) {
        Console.OutputEncoding = new UTF8Encoding(false);
        var json = args.TakeWhile(arg => arg != "--").Contains("--json");
        try {
            var options = Options.Parse(args);
            switch (options.Command) {
                case "help": Terminal.WriteLine(options.HelpText.TrimEnd()); return 0;
                case "version":
                    if (options.Json) WriteJson(writer => { writer.WriteStartObject(); writer.WriteString("version", Version); writer.WriteString("engine", "JavaScriptCore"); writer.WriteNumber("protocolVersion", Contract.ProtocolVersion); writer.WriteEndObject(); });
                    else Terminal.WriteLine($"ogma {Version} (JavaScriptCore)"); return 0;
                case "capabilities": Capabilities(); return 0;
            }
            var request = options.Request();
            if (options.DryRun) { Console.WriteLine(JsonSerializer.Serialize(request, ProtocolJson.Default.Request)); return 0; }
            if (options.Command == "mcp-serve") return await McpFrontend.Run(options);
            if (options.Command is "model-run" or "model-chat") return await ModelFrontend.Run(options);
            if (options.Command == "repl") return await Frontend.RunRepl(options);
            if (options.Command is "rpc" or "lsp") return await Frontend.RunRpc(options, options.Command == "lsp");
            if (options.Explain) Terminal.WriteLine(options.Trusted
                ? "Trusted ABI execution: native engine shares the CLI process; external process controls are unavailable."
                : "Rust runs this request in an isolated worker. Guest files, environment and network need explicit grants; tooling operates on the selected project. Timeout, cancellation and sampled resident-memory limits terminate the process tree without replay. --sandbox=linux opts into the fail-closed namespace/seccomp prototype.", error: true);
            using var cancellation = new CancellationTokenSource();
            ConsoleCancelEventHandler handler = (_, e) => { e.Cancel = true; cancellation.Cancel(); };
            Console.CancelKeyPress += handler;
            Response response;
            try { response = options.Trusted ? TrustedRuntime.Execute(request) : await Supervisor.ExecuteAsync(request, cancellation.Token); }
            finally { Console.CancelKeyPress -= handler; }
            Render(response, options.Json);
            return response.Status == "ok" ? 0 : 1;
        } catch (CliFailure error) { Render(Failure(error.Code, error.Message), json); return 1; }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or System.ComponentModel.Win32Exception or DllNotFoundException or EntryPointNotFoundException or JsonException) {
            Render(Failure("OGMA_IO", error.Message), json); return 1;
        }
    }
    private static Response Failure(string code, string message) => new() {
        ProtocolVersion = Contract.ProtocolVersion, Id = "cli", Status = "error",
        Diagnostics = [new() { Code = code, Message = message }]
    };
    internal static void Render(Response response, bool json) {
        if (json) { Console.WriteLine(JsonSerializer.Serialize(response, ProtocolJson.Default.Response)); return; }
        foreach (var item in response.Output) Terminal.WriteLine(item.Text, item.Stream == "stderr");
        foreach (var item in response.Diagnostics) Terminal.Diagnostic(item);
        foreach (var item in response.Trace) Terminal.Trace(item);
    }
    private static void WriteJson(Action<Utf8JsonWriter> write) {
        using var stream = new MemoryStream(); using (var writer = new Utf8JsonWriter(stream)) write(writer);
        Console.WriteLine(Encoding.UTF8.GetString(stream.ToArray()));
    }
    internal static void Capabilities() => WriteJson(writer => {
        var arch = System.Runtime.InteropServices.RuntimeInformation.ProcessArchitecture;
        var supported = OperatingSystem.IsLinux() && arch == System.Runtime.InteropServices.Architecture.X64
            || OperatingSystem.IsMacOS() && arch is System.Runtime.InteropServices.Architecture.X64 or System.Runtime.InteropServices.Architecture.Arm64;
        writer.WriteStartObject(); writer.WriteNumber("protocolVersion", Contract.ProtocolVersion);
        writer.WriteNumber("abiVersion", Contract.AbiVersion); writer.WriteString("version", Version); writer.WriteString("engine", "JavaScriptCore");
        writer.WriteString("engineBinding", "rust_jsc=0.5.0/rust_jsc_sys=0.4.2");
        writer.WriteBoolean("workerSupported", supported);
        writer.WriteString("isolation", "process"); writer.WriteBoolean("hardSandbox", false);
        writer.WriteString("pathEnforcement", !supported ? "unavailable" : (OperatingSystem.IsLinux() || OperatingSystem.IsMacOS()) ? "descriptor-no-symlinks" : "canonical-containment");
        writer.WriteStartArray("platforms"); writer.WriteStringValue("linux"); writer.WriteStringValue("macos"); writer.WriteEndArray();
        writer.WriteStartObject("features");
        foreach (var name in new[] { "typescript", "localEsm", "transformCache", "graphCache", "asyncFileIo", "sleep", "environment", "network", "testRunner", "trustedAbi", "tracing", "repl", "formatter", "linter", "bundler", "npmInstall", "typeChecker", "jsonRpc", "languageServer", "localModels", "modelChat", "modelEmbeddings", "mcpTools", "mcpStdio" }) writer.WriteBoolean(name, supported);
        writer.WriteBoolean("dryRun", true);
        writer.WriteBoolean("linuxSandboxPrototype", OperatingSystem.IsLinux() && supported);
        writer.WriteBoolean("releaseSigning", true);
        foreach (var name in new[] { "nodeCompatibility", "nativeAddons", "subprocess", "nativeLibraries", "snapshot", "engineCodeCache", "warmDaemon", "signedBuilds", "windowsWorker", "stableLanguageServer" }) writer.WriteBoolean(name, false);
        writer.WriteEndObject(); writer.WriteEndObject();
    });
}
