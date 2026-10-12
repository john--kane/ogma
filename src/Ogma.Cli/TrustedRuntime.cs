using System.Text.Json;
namespace Ogma;

internal static unsafe class TrustedRuntime {
    public static Response Execute(Request request) {
        if (NativeMethods.Ogma_abi_version() != Contract.AbiVersion) throw new CliFailure("OGMA_PROTOCOL", "Unsupported native ABI version");
        Check(NativeMethods.Ogma_create(Contract.AbiVersion, out var handle));
        try {
            var bytes = JsonSerializer.SerializeToUtf8Bytes(request, ProtocolJson.Default.Request);
            fixed (byte* pointer = bytes) Check(NativeMethods.Ogma_execute(handle, pointer, (nuint)bytes.Length));
            var status = NativeMethods.Ogma_response(handle, null, 0, out nuint size);
            if (status != Status.BufferTooSmall || size > Contract.MaxFrameBytes) throw new CliFailure("OGMA_ABI", $"Invalid response buffer: {status}");
            var output = new byte[(int)size];
            fixed (byte* pointer = output) Check(NativeMethods.Ogma_response(handle, pointer, size, out size));
            return ResponseValidation.Validate(JsonSerializer.Deserialize(output, ProtocolJson.Default.Response)
                ?? throw new CliFailure("OGMA_PROTOCOL", "Null native response"), request);
        } finally { Check(NativeMethods.Ogma_destroy(handle)); }
    }
    private static void Check(Status status) {
        if (status != Status.Ok) throw new CliFailure("OGMA_ABI", $"Native ABI returned {status}");
    }
}
