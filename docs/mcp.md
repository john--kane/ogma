# MCP tool servers

Ogma creates and hosts TypeScript tools over MCP stdio. The C# NativeAOT frontend
handles MCP lifecycle and JSON-RPC; the existing supervisor runs each discovery
or invocation in a fresh isolated Rust/JavaScriptCore worker. No model process is
needed.

```sh
ogma mcp init ./my-server
cd my-server
ogma check .
ogma mcp serve server.ts --allow-read=.
```

`init` creates a new directory with an echo tool, generated declarations and a
README. It refuses an existing directory, file or symlink. The explicit creation
command authorizes scaffolding; it does not grant guest filesystem access.
`--dry-run` prints the planned request without creating files or starting a server.

Register tools in `server.ts`:

```ts
/// <reference path="./ogma.d.ts" />
Ogma.mcpTool({
  name: "echo",
  description: "Return supplied text unchanged.",
  inputSchema: {
    type: "object",
    properties: { text: { type: "string" } },
    required: ["text"],
    additionalProperties: false,
  },
}, async (args) => String(args.text));
```

Handlers return `string` or `Promise<string>`; results become MCP text content.
Input schemas are validated by pinned `jsonschema` 0.58.6 with retrieval disabled.
Local schema references work; unresolved external file/network references fail.
Names contain ASCII letters, digits, `_`, `-`, or `.`, with a maximum of 128
characters. A module can register at most 128 tools; duplicates fail.

For a client supporting `mcpServers`, use absolute paths:

```json
{
  "mcpServers": {
    "ogma-example": {
      "command": "ogma",
      "args": [
        "mcp", "serve", "/absolute/path/to/my-server/server.ts",
        "--allow-read=/absolute/path/to/my-server"
      ]
    }
  }
}
```

Keep the complete Ogma distribution beside the CLI. The client starts Ogma and
owns the stdio connection; `serve` does not listen on a network port.
`--transport=stdio` is optional. HTTP transport is unavailable in this preview.

## Permissions and execution

Entry modules and imports require explicit read grants. Files, network and
environment remain denied by default; add only the needed `--allow-read`,
`--allow-write`, `--allow-net` or `--allow-env` grants. Client arguments cannot
change the CLI policy. Guest operations execute in Rust/JSC without going through
C#. `--trusted-in-process` is refused for MCP commands.

Every `tools/list` and `tools/call` reloads the module in a fresh worker. Keep
top-level code limited to registration. Module globals do not persist between
calls; use explicitly granted storage when persistence is necessary. Files are
revalidated under current grants even when transform/graph caches are populated.

Standard worker limits apply to each operation: a 30-second deadline, 256 MiB
sampled RSS and 1 MiB output by default. Configure `--timeout-ms`, `--memory-mb`
and `--max-output-bytes`; at most four operations run concurrently. RSS is sampled,
not a hard allocation cap. `--sandbox=linux` applies the existing opt-in Linux
prototype to execution workers; it is not a complete OS sandbox.

Cancellation, EOF and Ctrl+C terminate active workers. Crashes and timeouts never
replay calls. Filesystem effects completed before termination remain completed;
calls are not transactions. The frontend survives operation failures and accepts
subsequent requests. Client reconnection creates a new frontend.

## Protocol and preview limits

MCP uses newline-delimited UTF-8 JSON-RPC, distinct from Ogma's framed worker IPC
and Content-Length RPC/LSP frontend. Stdout carries MCP messages only; guest console
logs go to stderr. Input lines are bounded to 8 MiB, JSON depth to 64, tool
definitions/arguments to 1 MiB, and results share the guest output budget.
`--trace` writes worker phase timings/counts to stderr; `--explain` describes the
execution boundary there before serving.

The lifecycle is `initialize`, `notifications/initialized`, then `tools/list` and
`tools/call`. `ping` and `notifications/cancelled` are supported. Negotiated
versions are generated from `contracts/ogma.json`: `2025-11-25`, `2025-06-18`,
`2025-03-26`, and `2024-11-05`. Unsupported requested dates receive the newest
supported date; clients must check the response.

Invalid arguments/unknown tools return JSON-RPC `-32602`; handler errors return
text content with `isError: true`. Cancelled requests return `-32800`. Supervisor
failures include an Ogma diagnostic code in the JSON-RPC error data. Batches are
rejected; prompts, resources, sampling, streaming/progress and tool-list change
notifications are unavailable. Only tools are advertised during initialization.

The shared Ogma worker protocol is now **v4** (ABI remains v1). CLI and worker
must come from the same build. Historical v1–v3 schemas remain available; they
do not make mismatched workers compatible.
