using Spectre.Console;

namespace Ogma;

// Lazy console creation keeps terminal detection off JSON/RPC startup paths.
internal static class Terminal {
    private static readonly Lazy<IAnsiConsole> Output = new(() => Create(Console.Out, Console.IsOutputRedirected));
    private static readonly Lazy<IAnsiConsole> Error = new(() => Create(Console.Error, Console.IsErrorRedirected));

    private static IAnsiConsole Create(TextWriter writer, bool redirected) {
        var console = AnsiConsole.Create(new AnsiConsoleSettings {
            Out = new AnsiConsoleOutput(writer),
            Ansi = redirected ? AnsiSupport.No : AnsiSupport.Detect,
            Interactive = redirected ? InteractionSupport.No : InteractionSupport.Detect,
        });
        // Guest output is literal data. Preserve long lines when redirected or
        // printed to a terminal; the terminal itself handles visual wrapping.
        console.Profile.Width = int.MaxValue;
        return console;
    }
    public static void WriteLine(string text, bool error = false) => (error ? Error.Value : Output.Value).WriteLine(text);
    public static void Prompt(string text) => Error.Value.Write(text);
    public static void Diagnostic(Diagnostic diagnostic) {
        Error.Value.Write(diagnostic.Code + ": ", new Style(Color.Red, decoration: Decoration.Bold));
        Error.Value.WriteLine(diagnostic.Message);
    }
    public static void Trace(TraceEvent trace) => Error.Value.WriteLine($"trace {trace.Name} {trace.ElapsedMicros}us count={trace.Count}");
}
