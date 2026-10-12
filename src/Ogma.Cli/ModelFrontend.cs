namespace Ogma;

// Terminal input and IPC only. The worker owns model resolution, llama-server,
// HTTP, conversation history and inference; none of those execute in this UI.
internal static class ModelFrontend {
    public static async Task<int> Run(Options options) {
        var request = options.Request();
        using var cancellation = new CancellationTokenSource();
        ConsoleCancelEventHandler handler = (_, e) => { e.Cancel = true; cancellation.Cancel(); };
        Console.CancelKeyPress += handler;
        try {
            await using var session = new InteractiveSupervisor(request);
            var ready = await session.ExecuteAsync(request with { Id = "model-start" }, cancellation.Token);
            Program.Render(ready, options.Json);
            if (ready.Status != "ok") return 1;
            int sequence = 0;
            if (options.Command == "model-run") {
                while (true) {
                    await session.WaitWhileIdle(async token => {
                        await Task.Delay(1000, token); return true;
                    }, cancellation.Token);
                    var status = await session.ExecuteAsync(request with {
                        Command = "model-status", Id = $"model-status-{++sequence}"
                    }, cancellation.Token);
                    if (status.Status != "ok") { Program.Render(status, options.Json); return 1; }
                }
            }
            if (!Console.IsInputRedirected)
                Terminal.WriteLine("Ogma model chat. Enter a prompt; .exit ends the session.", error: true);
            while (true) {
                if (!Console.IsInputRedirected) Terminal.Prompt("> ");
                var line = await session.WaitWhileIdle(token => Console.In.ReadLineAsync(token).AsTask(), cancellation.Token);
                if (line is null || line.Trim() == ".exit") return 0;
                if (string.IsNullOrWhiteSpace(line)) continue;
                var response = await session.ExecuteAsync(request with {
                    Id = $"model-chat-{++sequence}", Source = line
                }, cancellation.Token);
                Program.Render(response, options.Json);
                if (response.Status != "ok") return 1;
            }
        } catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { return 0; }
        catch (CliFailure error) when (error.Code == "OGMA_CANCELLED" && cancellation.IsCancellationRequested) { return 0; }
        finally { Console.CancelKeyPress -= handler; }
    }
}
