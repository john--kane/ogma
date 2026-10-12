namespace Ogma;

internal static class ResponseValidation {
    public static Response Validate(Response response, Request request) {
        if (response.ProtocolVersion != Contract.ProtocolVersion || response.Id != request.Id
            || response.Status is not ("ok" or "error"))
            throw new CliFailure("OGMA_PROTOCOL", "Invalid response identity, version or status");
        // Nullable annotations do not reject null elements in JSON arrays.
        if (response.Output is null || response.Diagnostics is null || response.Trace is null
            || response.Output.Any(e => e is null || e.Stream is not ("stdout" or "stderr") || e.Text is null)
            || response.Diagnostics.Any(e => e is null || string.IsNullOrEmpty(e.Code) || e.Message is null || e.Specifier is null)
            || response.Trace.Any(e => e is null || e.Name is null)
            || (response.Status == "error" && response.Diagnostics.Length == 0)
            || (response.Status == "ok" && response.Diagnostics.Length != 0))
            throw new CliFailure("OGMA_PROTOCOL", "Invalid response events or diagnostics");
        return response;
    }
}
