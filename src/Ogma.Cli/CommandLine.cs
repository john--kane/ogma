using System.CommandLine;
using System.CommandLine.Help;
using System.CommandLine.Parsing;
using System.Globalization;

namespace Ogma;

internal sealed class CliFailure(string code, string message) : Exception(message) {
    public string Code { get; } = code;
}
internal sealed record Options {
    public required string Command { get; init; }
    public string Entry { get; init; } = "";
    public string Source { get; init; } = "";
    public string[] Args { get; init; } = [];
    public Permissions Permissions { get; init; } = new();
    public Limits Limits { get; init; } = new() { TimeoutMs = 30_000, MaxMemoryMb = 256, MaxOutputBytes = 1_048_576 };
    public ToolOptions Tool { get; init; } = new();
    public ModelOptions Model { get; init; } = new() { Ctx = 4096, Parallel = 1, Port = 8080 };
    public string Sandbox { get; init; } = "";
    public string HelpText { get; init; } = "";
    public bool Json { get; init; }
    public bool DryRun { get; init; }
    public bool Explain { get; init; }
    public bool Trace { get; init; }
    public bool Trusted { get; init; }
    public string CacheDirectory { get; init; } = "";

    public Request Request() => new() {
        ProtocolVersion = Contract.ProtocolVersion, Id = "cli", Command = Command,
        Entry = Entry, Source = Source, Args = Args, Permissions = Permissions, Limits = Limits,
        CacheDirectory = CacheDirectory, Trace = Trace, Tool = Tool, Sandbox = Sandbox, Model = Model
    };
    public static Options Parse(string[] args) {
        var root = new RootCommand("Ogma — JavaScriptCore JS/TS runtime preview");
        root.Options.Clear();
        root.SetAction(_ => 0); // Parsing returns Options; only generated help is invoked.
        root.Directives.Clear(); // Guest source must never act as a CLI directive.
        root.Options.Add(new HelpOption { Recursive = true });
        Option<T> Add<T>(Option<T> option) { option.Recursive = true; root.Options.Add(option); return option; }
        var jsonOption = Add(Flag("--json", "Stable JSON output"));
        var dryOption = Add(Flag("--dry-run", "Print the request without executing"));
        var explainOption = Add(Flag("--explain", "Explain the execution boundary"));
        var traceOption = Add(Flag("--trace", "Include structured timing events"));
        var trustedOption = Add(Flag("--trusted-in-process", "Opt into trusted ABI without process isolation"));
        var noCacheOption = Add(Flag("--no-cache", "Disable persistent caches"));
        var cacheOption = Add(Text("--cache-dir", "Dedicated transform/graph cache directory"));
        var timeoutOption = Add(Number("--timeout-ms", "Deadline: 1..600000 ms (default 30000)", 1, 600_000, 30_000));
        var memoryOption = Add(Number("--memory-mb", "Sampled RSS: 16..65536 MiB (default 256); above 4096 requires --allow-model", 16, 65536, 256));
        var outputOption = Add(Number("--max-output-bytes", "Maximum guest output: 1..1048576 bytes", 1, 1_048_576, 1_048_576));
        var readOption = Add(Grant("--allow-read", "Repeat for each explicit read root/file"));
        var writeOption = Add(Grant("--allow-write", "Repeat for each existing write root/file"));
        var envOption = Add(Grant("--allow-env", "Repeat for each environment name"));
        var netOption = Add(Grant("--allow-net", "Exact HTTP/HTTPS host[:port] grant; checked on redirects"));
        var subprocessOption = Add(Grant("--allow-subprocess", "Reserved: guest process API unavailable"));
        var nativeOption = Add(Grant("--allow-native", "Reserved: guest native-library API unavailable"));
        var sandboxOption = Add(Text("--sandbox", "Fail-closed Linux namespace/seccomp prototype (linux)"));
        var allowModelOption = Add(Flag("--allow-model", "Grant local model APIs; requires worker isolation"));
        var portOption = Add(Number("--port", "Existing local model service port", 1, 65535, 8080));
        var compactOption = Add(Flag("--caveman", "Compact model output system instruction"));
        var ultraOption = Add(Flag("--caveman-ultra", "Ultra compact model output system instruction"));
        var versionOption = Add(Flag("--version", "Print version", "-V"));
        var writeToolOption = Flag("--write", "Apply formatting/lint fixes");
        var lockedOption = Flag("--locked", "Verify offline store against the lockfile");
        var toolOutputOption = Text("--output", "Bundle output file (required)");
        var registryOption = Text("--registry", "npm registry", "https://registry.npmjs.org");
        var storeOption = Text("--store", "npm content-addressed store");
        var modelOption = Text("--model", "Select a model for model ask");
        var serverOption = Text("--llama-server", "Trust an explicit llama-server executable");
        var modelStoreOption = Text("--model-store", "Dedicated model cache");
        var contextOption = Text("--context", "Reference context file (requires read grant)");
        var ctxOption = Number("--ctx", "Context per parallel slot", 1, 1_048_576, 4096);
        var parallelOption = Number("--parallel", "Parallel model slots", 1, 1024, 1);
        var gpuOption = new Option<int>("--gpu-layers") {
            Description = "GPU layers (-1 means all)", Arity = ArgumentArity.ExactlyOne,
            CustomParser = result => {
                if (result.Tokens.Count == 1 && int.TryParse(result.Tokens[0].Value, NumberStyles.Integer, CultureInfo.InvariantCulture, out int n) && n >= -1) return n;
                result.AddError("--gpu-layers must be -1 or a nonnegative integer"); return 0;
            }, DefaultValueFactory = _ => 0
        };
        var commands = new Dictionary<Command, (string Name, Argument<string>? Input)>();
        Command Command(string name, string description, string? argument = null, bool optional = false, string fallback = "", Command? parent = null, string? wireName = null) {
            var command = new Command(name, description);
            Argument<string>? input = null;
            if (argument is not null) {
                input = new Argument<string>(argument) {
                    Arity = optional ? ArgumentArity.ZeroOrOne : ArgumentArity.ExactlyOne,
                    CustomParser = r => r.Tokens.Count == 0 ? fallback : r.Tokens[0].Value,
                    DefaultValueFactory = _ => fallback
                };
                command.Arguments.Add(input);
            }
            (parent ?? root).Subcommands.Add(command);
            commands.Add(command, (wireName ?? name, input));
            return command;
        }
        Command("run", "Execute a JS/TS module; guest arguments follow --", "main.ts");
        Command("eval", "Evaluate exact JS/TS source", "source");
        Command("test", "Run native guest tests", "tests.ts");
        foreach (var name in new[] { "fmt", "lint" }) {
            var cmd = Command(name, name == "fmt" ? "Format with pinned Biome" : "Lint with pinned Biome", "path", true, ".");
            cmd.Options.Add(writeToolOption);
        }
        var bundle = Command("bundle", "Bundle through pinned Rolldown", "entry.ts");
        bundle.Options.Add(toolOutputOption);
        Command("check", "Run the off-path TypeScript checker", "entry.ts|project", true, ".");
        var install = Command("install", "Install the supported pure-ESM npm subset", "project", true, ".");
        install.Options.Add(lockedOption); install.Options.Add(registryOption); install.Options.Add(storeOption);
        Command("repl", "Start a persistent JS/TS session");
        Command("rpc", "Serve framed JSON-RPC over stdin/stdout");
        Command("lsp", "Serve preview language-server diagnostics");
        Command("capabilities", "Report versioned runtime capabilities");
        Command("version", "Print version");
        Command("help", "Show command help");
        var mcpGroup = Command("mcp", "Create and host TypeScript MCP tool servers");
        Command("init", "Create a new MCP server project without overwriting", "directory", parent: mcpGroup, wireName: "mcp-init");
        var mcpServe = Command("serve", "Host an MCP tools server over newline-delimited JSON stdio", "server.ts", parent: mcpGroup, wireName: "mcp-serve");
        var transportOption = Text("--transport", "MCP transport (stdio)", "stdio");
        mcpServe.Options.Add(transportOption);
        var modelGroup = Command("model", "Local model hosting");
        foreach (var option in new Option[] { modelOption, serverOption, modelStoreOption, contextOption, ctxOption, parallelOption, gpuOption }) {
            option.Recursive = true; modelGroup.Options.Add(option);
        }
        Command("run", "Host a local model", "model.gguf|HFid", parent: modelGroup, wireName: "model-run");
        Command("chat", "Interactive model chat", "model.gguf|HFid", true, parent: modelGroup, wireName: "model-chat");
        Command("ask", "One-shot model inference", "prompt", parent: modelGroup, wireName: "model-ask");

        // Core -- is exclusively a guest argv boundary. Model -- still quotes a
        // positional prompt/model. No response files or directive actions run.
        int separator = Array.IndexOf(args, "--");
        bool modelSurface = args.FirstOrDefault() == "model";
        var parseArgs = separator >= 0 && !modelSurface ? args[..separator] : args;
        var scriptArgs = separator >= 0 && !modelSurface ? args[(separator + 1)..] : [];
        var parsed = root.Parse(parseArgs, new ParserConfiguration { ResponseFileTokenReplacer = null, EnablePosixBundling = false });
        var selected = parsed.CommandResult.Command;
        if (parsed.Action is HelpAction || args.Length == 0 || selected.Name == "help") {
            using var writer = new StringWriter(CultureInfo.InvariantCulture);
            var help = parsed.Action is HelpAction ? parsed : root.Parse(["--help"]);
            help.Invoke(new InvocationConfiguration { Output = writer, Error = writer, EnableDefaultExceptionHandler = false });
            return new Options { Command = "help", HelpText = writer.ToString() };
        }
        if (parsed.Errors.Count != 0) {
            bool unknownCommand = parseArgs.Length > 0 && !parseArgs[0].StartsWith('-') && !root.Subcommands.Any(c => c.Name == parseArgs[0]);
            throw new CliFailure(unknownCommand ? "OGMA_UNSUPPORTED" : "OGMA_INVALID_ARGUMENT", string.Join("; ", parsed.Errors.Select(e => e.Message)));
        }
        bool Has(Option option) => parsed.GetResult(option) is { Implicit: false };
        if (selected == root && !parsed.GetValue(versionOption)) throw new CliFailure("OGMA_INVALID_ARGUMENT", "A command is required");
        var command = parsed.GetValue(versionOption) ? "version" : commands[selected].Name;
        if (command == "mcp") throw new CliFailure("OGMA_INVALID_ARGUMENT", "mcp requires init or serve");
        if (command == "model") throw new CliFailure("OGMA_INVALID_ARGUMENT", "model requires run, chat or ask");
        if (command == "mcp-serve" && parsed.GetValue(transportOption) != "stdio") throw new CliFailure("OGMA_UNSUPPORTED", "MCP currently supports stdio transport");
        if (command.StartsWith("mcp-", StringComparison.Ordinal) && (parsed.GetValue(trustedOption) || Has(portOption))) throw new CliFailure("OGMA_INVALID_ARGUMENT", "MCP requires isolated workers and stdio; --trusted-in-process and --port do not apply");
        bool modelCommand = command.StartsWith("model-", StringComparison.Ordinal);
        var positional = commands.TryGetValue(selected, out var spec) && spec.Input is not null ? parsed.GetValue(spec.Input)! : "";
        if (scriptArgs.Length != 0 && command is not ("run" or "eval" or "test" or "fmt" or "lint" or "bundle" or "check" or "install"))
            throw new CliFailure("OGMA_INVALID_ARGUMENT", "Unexpected script arguments");
        List<string> Paths(Option<string[]> option) => parsed.GetValue(option)!.Select(Path.GetFullPath).ToList();
        var read = Paths(readOption); var write = Paths(writeOption);
        var env = parsed.GetValue(envOption)!; var net = parsed.GetValue(netOption)!;
        var subprocess = Paths(subprocessOption); var native = Paths(nativeOption);
        if (env.Any(name => name.Contains('=') || name.Contains('\0'))) throw new CliFailure("OGMA_INVALID_ARGUMENT", "Invalid environment name");
        string Full(Option<string> option) => parsed.GetValue(option) is { Length: > 0 } value ? Path.GetFullPath(value) : "";
        bool json = parsed.GetValue(jsonOption), dry = parsed.GetValue(dryOption), explain = parsed.GetValue(explainOption), trace = parsed.GetValue(traceOption), trusted = parsed.GetValue(trustedOption), allowModel = parsed.GetValue(allowModelOption);
        uint timeout = modelCommand && !Has(timeoutOption) ? 120_000u : parsed.GetValue(timeoutOption);
        uint memory = modelCommand && !Has(memoryOption) ? 4096u : parsed.GetValue(memoryOption), output = parsed.GetValue(outputOption);
        string cache = parsed.GetValue(noCacheOption) ? "" : Has(cacheOption) ? Full(cacheOption) : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ogma", "cache", "v1");
        string sandbox = parsed.GetValue(sandboxOption)!;
        if (sandbox.Length != 0 && sandbox != "linux") throw new CliFailure("OGMA_UNSUPPORTED", "Only the Linux sandbox prototype is available");
        bool writeTool = parsed.GetValue(writeToolOption), locked = parsed.GetValue(lockedOption);
        string toolOutput = Full(toolOutputOption), registry = command == "install" ? parsed.GetValue(registryOption)! : "https://registry.npmjs.org", store = Full(storeOption);
        var model = new ModelOptions { Model = parsed.GetValue(modelOption) ?? "", Server = Full(serverOption), Store = Full(modelStoreOption), Context = Full(contextOption), Ctx = modelCommand ? parsed.GetValue(ctxOption) : 4096, Parallel = modelCommand ? parsed.GetValue(parallelOption) : 1, GpuLayers = parsed.GetValue(gpuOption), Port = parsed.GetValue(portOption) };
        if (parsed.GetValue(compactOption) && parsed.GetValue(ultraOption)) throw new CliFailure("OGMA_INVALID_ARGUMENT", "Choose only one caveman mode");
        model = model with { Caveman = parsed.GetValue(ultraOption) ? "ultra" : parsed.GetValue(compactOption) ? "compact" : "" };
        if (command is "model-run" or "model-chat" && positional.Length != 0) {
            if (model.Model.Length != 0) throw new CliFailure("OGMA_INVALID_ARGUMENT", "Model selected twice");
            model = model with { Model = positional };
        }
        if ((ulong)model.Ctx * model.Parallel > 1_048_576)
            throw new CliFailure("OGMA_INVALID_ARGUMENT", "Total model context across parallel slots exceeds 1048576");
        if (memory > 4096 && !allowModel)
            throw new CliFailure("OGMA_INVALID_ARGUMENT", "Memory above 4096 MiB requires --allow-model");
        if (modelCommand && !allowModel)
            throw new CliFailure("OGMA_PERMISSION_DENIED", "Model operations require --allow-model");
        if (allowModel && (trusted || sandbox.Length != 0))
            throw new CliFailure("OGMA_INVALID_ARGUMENT", "Model permission requires an isolated worker without --sandbox=linux");
        if (trusted && (sandbox.Length != 0 || command is "repl" or "rpc" or "lsp")) throw new CliFailure("OGMA_INVALID_ARGUMENT", "Sandbox and interactive frontends require process isolation");
        if (sandbox.Length != 0 && command is "fmt" or "lint" or "bundle" or "check" or "install" or "mcp-init") throw new CliFailure("OGMA_UNSUPPORTED", "The sandbox prototype applies to guest execution, not tool adapters");
        if (command == "bundle" && toolOutput.Length == 0) throw new CliFailure("OGMA_INVALID_ARGUMENT", "bundle requires --output=<file>");
        if (trusted && (memory != 256 || timeout != 30_000)) throw new CliFailure("OGMA_INVALID_ARGUMENT", "External memory/time controls require process isolation");
        if (command == "check") {
            var target = Path.GetFullPath(positional);
            read.Add(Directory.Exists(target) ? target : Path.GetDirectoryName(target)!);
        }
        return new Options {
            Command = command, Entry = command is "mcp-init" or "mcp-serve" or "run" or "test" or "fmt" or "lint" or "bundle" or "check" or "install" ? Path.GetFullPath(positional) : "",
            Source = command is "eval" or "model-ask" ? positional : "", Args = scriptArgs,
            Permissions = new() { Read = read.ToArray(), Write = write.ToArray(), Env = env.ToArray(), Net = net.ToArray(), Subprocess = subprocess.ToArray(), Native = native.ToArray(), Model = allowModel },
            Limits = new() { TimeoutMs = timeout, MaxMemoryMb = memory, MaxOutputBytes = output },
            Model = model, Tool = new() { Write = writeTool, Locked = locked, Output = toolOutput, Registry = registry, Store = store }, Sandbox = sandbox,
            Json = json, DryRun = dry, Explain = explain, Trace = trace, Trusted = trusted, CacheDirectory = cache
        };
    }
    private static Option<bool> Flag(string name, string description, params string[] aliases) => new(name, aliases) {
        Description = description, Arity = ArgumentArity.Zero,
        CustomParser = result => {
            if (result.Tokens.Count != 0) result.AddError($"{name} takes no value");
            return true;
        }, DefaultValueFactory = _ => false
    };
    private static Option<string> Text(string name, string description, string fallback = "") => new(name) {
        Description = description, Arity = ArgumentArity.ExactlyOne,
        CustomParser = result => {
            if (result.Tokens.Count == 1 && result.Tokens[0].Value.Length != 0 && !result.Tokens[0].Value.StartsWith('-')) return result.Tokens[0].Value;
            result.AddError($"{name} requires an explicit nonempty value"); return "";
        }, DefaultValueFactory = _ => fallback
    };
    private static Option<string[]> Grant(string name, string description) => new(name) {
        Description = description, Arity = ArgumentArity.OneOrMore, AllowMultipleArgumentsPerToken = false,
        CustomParser = result => {
            var values = result.Tokens.Select(t => t.Value).ToArray();
            if (values.Length == 0 || values.Any(v => string.IsNullOrEmpty(v) || v.StartsWith('-')) ||
                result.Parent is OptionResult option && values.Length != option.IdentifierTokenCount)
                result.AddError($"{name} requires one explicit nonempty value per occurrence; use ./ for paths beginning with '-'");
            return values;
        }, DefaultValueFactory = _ => []
    };
    private static Option<uint> Number(string name, string description, uint min, uint max, uint fallback) => new(name) {
        Description = description, Arity = ArgumentArity.ExactlyOne,
        CustomParser = result => {
            if (result.Tokens.Count == 1 && uint.TryParse(result.Tokens[0].Value, NumberStyles.None, CultureInfo.InvariantCulture, out uint n) && n >= min && n <= max) return n;
            result.AddError($"{name} must be in {min}..{max}"); return 0;
        }, DefaultValueFactory = _ => fallback
    };
}
