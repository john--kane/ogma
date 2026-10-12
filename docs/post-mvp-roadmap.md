# Post-MVP roadmap: web and AI development

This is a proposed backlog, not a claim of implemented commands. Priorities
follow the workflows Ogma should make easier: build a web application, call an
AI API with streaming output, expose tools through MCP, and reproduce the result
in CI. Bun's supplied command/help listing is the reference; parity is not the
acceptance criterion.

## Starting point

Ogma already provides TS/ESM execution, eval, a REPL, native tests, fmt/lint,
type checking, Rolldown bundling, a locked pure-ESM npm subset, JSON diagnostics,
generated contracts, and MCP stdio scaffolding/hosting. These need targeted
extensions rather than replacement commands.

The current npm subset rejects CommonJS, peers, optional dependencies,
workspaces and lifecycle scripts. The guest has `Ogma.fetchText`, not a complete
Fetch/Streams API or an HTTP server. MCP tools currently return text and use a
fresh worker per call. Framework and AI SDK compatibility must be established
with actual packages and integration tests before advertising support.

Local model hosting remains outside this roadmap's first releases. Cloud AI
API clients, streamed web endpoints and MCP are the initial AI development focus.

## First release: run a useful web/AI service

| Priority | Capability / proposed surface | Bun inspiration | Useful completion proof |
| --- | --- | --- | --- |
| 1 | Web APIs: `fetch`, Request/Response/Headers, AbortController, byte/text encoding and ReadableStream; request bodies and incremental responses | Runtime fetch and streaming support | A pinned AI SDK sends a request, consumes SSE incrementally and cancels it; redirects still recheck grants and buffers remain bounded |
| 2 | HTTP serving through a generated JS/TS API such as `Ogma.serve`; Rust/Tokio owns sockets and handler execution | `Bun.serve`, `--port` | A TS service exposes a JSON route, streams model-provider output to a browser, detects disconnects and shuts down cleanly |
| 3 | Broaden install/resolution only to unlock selected web/AI packages: peer dependency resolution, optional dependency selection, package conditions, then required CommonJS interop | `install`, `--conditions`, resolution flags | Pinned representative AI client/schema packages install deterministically, run offline after installation, and have documented compatibility boundaries |
| 4 | `run --watch`, `test --watch`; restart on granted module/config changes with debounce and clear diagnostics | `--watch`, `--watch-kill-signal`, `--no-clear-screen` | Change a dependency, observe one deliberate restart, verify old descendants exit and failed calls are never replayed |
| 5 | `ogma init --template=api\|ai-api\|mcp\|web`; start with built-in, versioned templates | `init`, `create` | Each template installs/checks/tests/builds from a clean directory and includes generated types, explicit permission examples and a reproducible lockfile |
| 6 | Extend `bundle` for browser targets, TSX/JSX, assets, CSS, source maps, development/production modes and splitting where supported by the pinned adapter | `build`, JSX/loader/define flags | A small browser application builds and calls the Ogma API; production output loads without a dev server |

HTTP serving needs an explicit inbound-listen grant, separate from outbound
`--allow-net`, with loopback binding by default. The exact flag/API names need a
design before implementation. Long-lived service lifetime and per-request
deadlines must be separate; the current one-shot worker timeout cannot simply
be reused as the lifetime of a dev server. Streaming must honor backpressure,
body/header limits, cancellation and client disconnects.

Choose compatibility fixtures first: one browser application plus a minimal
API, one streaming AI provider SDK, and one schema/tool-definition library.
Expand support from failures in those fixtures. CommonJS and selected Node APIs
are bounded compatibility work, not a commitment to emulate all of Node.
Existing pinned Node tooling adapters can support build tools explicitly; Ogma
must not silently switch guest execution to Node.

## Second release: reduce daily workflow friction

| Priority | Capability / proposed surface | Bun inspiration | Ogma-specific scope |
| --- | --- | --- | --- |
| 7 | `ogma add`, `remove`, `update`, `outdated`, `info`, `why`; `audit` once the selected registry supports it | Package management commands | Deterministic manifest/lock updates, explainable dependency paths, dry-run changes and structured JSON. Network use is explicit; audit reports advisory-source coverage and freshness |
| 8 | `ogma task <name>` for package.json scripts, followed by named multi-task execution | `run <script>`, `--parallel`, `--sequential`, `--if-present`, `--no-exit-on-error` | Keep file execution unambiguous. Start with supervised processes/system shells and explicit executable/env grants; preserve individual exit codes and kill descendants. Do not build a shell language |
| 9 | `ogma x <package>@<version> [args]` for package CLIs | `x` / bunx | Require an installed locked binary or an explicit acquisition step. Separate package download from executable permission; verify integrity, supervise execution and never install silently during `run` |
| 10 | `ogma dev` orchestrates browser serving/build watching and a TS API worker | Web development workflow, watch/build | Reuse serving, bundling and task supervision. Provide readiness/port-conflict diagnostics and predictable teardown; avoid separate mechanisms for each template |
| 11 | Test discovery, filters, hooks, assertions, watch mode and JSON/JUnit results; add coverage after checking JSC support | `test` | Prioritize HTTP, streaming, permission and MCP integration tests. Consider snapshots/mocks after fixture needs establish their value |
| 12 | MCP resources, structured tool output and Streamable HTTP; client/server inspection and smoke testing | Ogma extension beyond Bun's command list | Retain stdio. HTTP requires an explicit bind policy, authentication/origin controls and bounded sessions. Add stateful sessions only for demonstrated tool needs, with documented crash semantics |

Package mutation commands should extend the existing Rust installer/store and
publication journal. Network-dependent resolution and offline locked execution
remain distinct. Native addons and unrestricted install hooks stay excluded;
optional dependencies must be selected/rejected according to target support,
not silently treated as functional.

## Third release: workspaces, diagnostics and distribution

| Capability | Bun inspiration | Priority rationale / prerequisite |
| --- | --- | --- |
| Workspace install and task filtering, `--filter`, `--workspaces`, local workspace references | Workspace flags, `link` / `unlink` | Useful for UI + API + shared schema repositories, after single-project install/task semantics are reliable. Prefer lock-recorded workspace references; do not weaken handle-based enforcement with arbitrary symlinks |
| Project configuration, `--cwd`, explicit `--env-file`, selected resolver/JSX settings | Config, cwd, env-file and tsconfig flags | Reduce repeated CLI options with documented precedence. Explicit env-file loading must respect read and env grants. Project configuration can request permissions but cannot grant them without host authorization |
| `ogma doctor --json`, dependency explanations, profile summaries | `info`, `why`, CPU/heap markdown profiles | Report engine/tool versions, compatibility gaps and actionable diagnostic codes. Start with existing phase/resource traces; expose CPU/heap profiling only after verifying maintained JSC APIs |
| Breakpoints and source-mapped debugging | `--inspect`, `--inspect-wait`, `--inspect-brk` | Investigate JSC inspector support first. A debugger endpoint needs explicit access and must not become an ambient execution channel |
| Permission presets and AI-friendly task/tool manifests | Cross-cutting improvement | Make required grants, schemas, outputs and exit behavior discoverable. Reuse shared contract generation and `capabilities`, `--json`, `--dry-run`, `--explain`; do not duplicate metadata per interface |
| Verified `ogma upgrade`, signed preview/stable channels and release provenance | `upgrade`, `--revision` | After real signing/distribution exists: authenticate updates, reject downgrades, publish atomically and preserve rollback. Resolve macOS verification and the tracked Windows JSC blocker separately |

## Defer or omit

- Full Node compatibility, native addons, FFI/C compilation and a runtime
  substitution flag like `--bun`. Adopt only compatibility required by chosen
  web/AI fixtures; report unsupported packages clearly.
- A custom shell and unrestricted `exec`. Named tasks and package binaries cover
  the initial workflows with fewer semantics to maintain.
- Automatic install or implicit latest-version lookup during execution. Use
  explicit installation and deterministic locks; avoid importing network and
  supply-chain side effects into `run`.
- Arbitrary remote project generators. Start with built-in templates; third-party
  generators later use the same explicit package/execution boundaries as `x`.
- `publish`, package patching, broad `pm` utilities, dedupe/prune and global
  link/unlink commands until maintaining distributable packages becomes a
  demonstrated user workflow. Internal resolver improvements need no new command.
- Hot module replacement across server state. Start with supervised watch
  restarts; add browser HMR through an adapter only when it materially improves
  the supported frontend workflow.
- SQL/Redis clients and startup preconnections. Standard package APIs are the
  first option; preconnections must never bypass network grants.
- Insecure HTTP parsing, obsolete TLS versions, blanket warning suppression,
  and flags exposing unsupported engine internals.
- A `--smol` mode, snapshots, persistent engine code caches or warm daemons before
  supported mechanisms and measured end-to-end gains justify them.

## Delivery rules

Keep C# NativeAOT as the user-facing command/protocol frontend; Rust owns
execution, permissions, async I/O, caches, adapters and process management;
JSC runs guest JS/TS. Generate new cross-language contracts and guest bindings
from the shared source. Preserve process isolation and deny-by-default access.

Every release needs real web/AI workflow fixtures, deterministic offline reruns,
cancellation/crash tests and cold/warm launch measurements. Also measure time to
first HTTP response, time to first streamed token, stream memory/backpressure
and watch restart latency. Changes to permissions, IPC or package resolution
need their affected boundary tests. Watch mode authorizes source-change restarts;
it does not authorize silent retry of failed requests after a crash.

Recommended first milestone: **a scaffolded browser + TS API application with
streaming AI calls, an MCP tool, watch mode, explicit grants and a locked CI run**.
That workflow should guide command selection and compatibility work.
