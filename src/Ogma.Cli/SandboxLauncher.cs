using System.Diagnostics;
namespace Ogma;

// Optional prototype, never a transparent fallback. Rust additionally checks
// namespace state and installs seccomp before creating JSC or Tokio threads.
internal static class SandboxLauncher {
    private const string Bubblewrap = "/usr/bin/bwrap";

    public static void Configure(ProcessStartInfo start, Request request, string worker) {
        if (request.Sandbox.Length == 0) return;
        if (request.Sandbox != "linux") throw new CliFailure("OGMA_INVALID_ARGUMENT", "Unknown sandbox mode");
        if (!OperatingSystem.IsLinux() || !File.Exists(Bubblewrap))
            throw Unavailable("Linux bubblewrap at /usr/bin/bwrap is required");
        if (request.Command is not ("run" or "eval" or "test" or "repl"))
            throw new CliFailure("OGMA_INVALID_ARGUMENT", "The Linux sandbox prototype supports guest execution commands only");
        Probe();
        start.FileName = Bubblewrap;
        start.ArgumentList.Clear();
        Add(start, "--unshare-all", "--as-pid-1", "--die-with-parent", "--new-session",
            "--cap-drop", "ALL", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp");
        // Shared loader/runtime data is read-only. The host root, home and temp
        // trees are absent except for explicit grants and the dedicated cache.
        foreach (var path in new[] { "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc/ld.so.cache", "/etc/localtime" })
            if (File.Exists(path) || Directory.Exists(path)) Bind(start, path, false);
        Bind(start, Path.GetDirectoryName(Path.GetFullPath(worker))!, false);
        var mounts = new Dictionary<string, bool>(StringComparer.Ordinal);
        foreach (var path in request.Permissions.Read) mounts[Path.GetFullPath(path)] = false;
        foreach (var path in request.Permissions.Write) mounts[Path.GetFullPath(path)] = true;
        if (request.CacheDirectory.Length != 0) {
            var cache = Path.GetFullPath(request.CacheDirectory);
            Directory.CreateDirectory(cache);
            mounts[cache] = true;
        }
        foreach (var (path, write) in mounts.OrderBy(e => e.Key.Length).ThenBy(e => e.Key, StringComparer.Ordinal)) {
            if (path == "/" || path == "/proc" || path.StartsWith("/proc/", StringComparison.Ordinal)
                || path == "/dev" || path.StartsWith("/dev/", StringComparison.Ordinal))
                throw new CliFailure("OGMA_INVALID_ARGUMENT", "Sandbox grants cannot replace root, /proc or /dev");
            if (!File.Exists(path) && !Directory.Exists(path))
                throw new CliFailure("OGMA_INVALID_ARGUMENT", $"Sandbox grant must exist: {path}");
            Bind(start, path, write);
        }
        var cwd = Directory.GetCurrentDirectory();
        Add(start, "--dir", cwd, "--chdir", cwd, "--", Path.GetFullPath(worker));
    }

    private static void Bind(ProcessStartInfo start, string path, bool write) {
        var parent = Path.GetDirectoryName(path);
        if (parent is { Length: > 0 } && parent != "/") Add(start, "--dir", parent);
        Add(start, write ? "--bind" : "--ro-bind", path, path);
    }
    private static void Add(ProcessStartInfo start, params string[] values) {
        foreach (var value in values) start.ArgumentList.Add(value);
    }
    private static CliFailure Unavailable(string reason) => new("OGMA_SANDBOX_UNAVAILABLE", $"Linux sandbox unavailable; execution was not started: {reason}");
    private static void Probe() {
        var info = new ProcessStartInfo(Bubblewrap) {
            UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true,
        };
        info.Environment.Clear();
        Add(info, "--unshare-all", "--as-pid-1", "--die-with-parent", "--new-session",
            "--cap-drop", "ALL", "--ro-bind", "/", "/", "--", "/usr/bin/true");
        using var probe = Process.Start(info) ?? throw Unavailable("namespace probe could not start");
        var errors = probe.StandardError.ReadToEndAsync();
        var output = probe.StandardOutput.ReadToEndAsync();
        if (!probe.WaitForExit(5000)) {
            probe.Kill(entireProcessTree: true);
            probe.WaitForExit();
            throw Unavailable("namespace probe exceeded five seconds");
        }
        if (probe.ExitCode != 0) throw Unavailable(errors.GetAwaiter().GetResult().Trim());
        output.GetAwaiter().GetResult();
    }
}
