# MVP verification record

Verified 2026-10-11T10:56:09.598827+00:00 on Linux-7.0.0-38-generic-x86_64-with-glibc2.43 (x86_64).

The executable distribution is `artifacts/`; the unsigned archive and digest manifest are in `dist/`. The engine override is JavaScriptCore. This record describes tested Linux behavior; macOS execution, Windows worker support and production signing are not inferred from Linux results.

## Historical baseline checks

| Check | Result |
| --- | --- |
| Optimized Rust worker/shared library build | Passed, locked dependencies |
| NativeAOT CLI publish | Passed, .NET 10 frontend |
| Python native acceptance suite | **66 passed**, 0 failures |
| Rust suite | **18 passed**, 0 failures; 1 irreversible-filter helper intentionally ignored and invoked by the passing subprocess test |
| Clippy all targets with warnings denied | Passed |
| Rust formatting | Passed |
| Generated contract drift | Passed |
| Draft 2020-12 protocol schema | Valid; five native command requests and final traced response conform |
| Ed25519 packaging/verification tests | Passed with disposable keys, including wrong key, tamper, downgrade and exact signed metadata substitution |

Native acceptance covers real Biome/Rolldown/TypeScript adapters, persistent REPL, RPC cancellation/fixed permissions, unsaved LSP diagnostics, verified offline install, hostile archives, install-publication interruption and recovery, redirect permissions, network response bounds, source/graph cache revalidation, Unicode/native ABI boundaries, no-replay crashes and process/descendant resource controls. Actual JSC runs under the seccomp filter in a Rust subprocess test.

## Requirement coverage

| Requested area | Implemented preview behavior |
| --- | --- |
| Native frontend, contracts and IPC/ABI | C# NativeAOT commands/diagnostics/public API, Rust worker, generated protocol v3/ABI v1; isolated default and trusted opt-in |
| Native TypeScript/ESM UX | SWC transforms/maps, local ESM/await/import; full checker only when requested |
| Permissions and async APIs | Deny-by-default fs/env/net, shared Unix handle traversal (Linux verified; macOS CI gate), Tokio file/sleep/HTTP text operations; reserved unavailable process/native grants |
| Startup/cache path | Fresh JSC safe builtins, atomic verified transform/content cache and source-validated persistent graph metadata; C# remains off guest execution path |
| Tools | Pinned Biome 2.5.15, Rolldown 1.2.13, TypeScript 6.0.3 and private Node 26.8.2; native guest tests |
| npm subset | Deterministic lock, SHA-512 store, offline locked install, bounded safe extraction, ESM resolver and fail-closed publication journal |
| Interactive interfaces | One-worker REPL, bounded/cancellable JSON-RPC, C# LSP diagnostics frontend with unsaved text and four-check concurrency bound |
| Fault/resource controls | Deadlines, cancellation, process-tree termination, sampled RSS, Unix CPU/core/descriptor limits, cache/crash/reply/diagnostic boundary tests; no replay |
| Sandbox | Opt-in fail-closed Linux namespace/seccomp prototype; no complete OS sandbox or cgroup hard limit claimed |
| Tracing/schema/declarations | Bounded named timing/count events, versioned strict schemas, generated bindings and declarations, capabilities/explain/dry-run |
| Preview distribution | Deterministic archive/digest tools, caller-key Ed25519 verification, Linux/macOS CI and user-selected CI signing secret; no publishing |

## Historical startup measurements

Final release binaries were measured after acceptance tests completed, with 30 samples per mode. Full raw samples, CPU/OS details and binary SHA-256 hashes are in [linux-mvp.json](benchmarks/linux-mvp.json).

| Mode | Median | p95 |
| --- | ---: | ---: |
| CLI capabilities only | 2.05 ms | 2.31 ms |
| Fresh worker, empty transform/graph caches | 8.85 ms | 9.88 ms |
| Fresh worker, populated transform/graph caches | 8.16 ms | 8.95 ms |

These are launch-to-exit measurements of a two-module TS workload. OS page caches are uncontrolled. There is no snapshot, persistent engine bytecode cache, daemon or cross-runtime speed claim.

## Material limits and external blockers

- Maintained JSC APIs expose no supported snapshot/persistent engine-code-cache mechanism. V8-specific requirements were superseded by the user's JSC selection.
- Windows worker support is the user-accepted JSC binding blocker. macOS Intel/Apple Silicon CI is configured, with execution and filesystem durability unverified locally.
- Combined namespaces/seccomp/JSC execution now passes through authorized Linux host execution; the tool sandbox still denies namespaces and fails closed. CI requires combined success. The filter remains a prototype deny-list.
- RSS is sampled rather than a hard cgroup cap. macOS now shares Unix handle-based file grants, but actual macOS execution and its descendant monitor remain unverified locally.
- REPL declarations are mutable globals with limited const/TDZ semantics; submissions are single-line and exports unsupported. LSP provides preview diagnostics only.
- Interrupted npm publication requires explicit operator restoration/removal of the journal. Bare package imports/install fail closed; ordinary explicit filesystem reads retain their normal grant semantics.
- The release workflow uses `OGMA_ED25519_PRIVATE_KEY` as selected by the user. No production identity is available here, remote secret provisioning is unverified, and the local distribution is unsigned. Signed test artifacts never establish a release identity.
- Full Node compatibility, native addons, complete OS sandboxing and stable LSP remain intentionally outside the MVP. Optional warm daemon is not implemented.

## Subsequent CLI migration

The current `artifacts/ogma` now uses pinned Spectre.Console 0.57.2 for human terminal output, including REPL output and diagnostics. NativeAOT publish with warnings treated as errors passed, and all **69 Python acceptance tests** passed, including literal markup brackets, long Unicode lines and clean JSON output. Generated contract drift remains clean. The startup hashes/measurements and `dist/` archive above describe the earlier MVP baseline, not this newer frontend.

The parser migration below completes this step using System.CommandLine while preserving NativeAOT. Spectre.Console.Cli 0.57.2 explicitly marks trimming/NativeAOT unsupported; no compatibility warnings were suppressed.

## Local model preview — 2026-10-11

Added C# NativeAOT model run/chat/ask over isolated Rust workers and pinned
llama.cpp v0.6.0, with generated protocol v3 (ABI v1 unchanged). The Rust proxy
owns loopback OpenAI chat/SSE/embeddings, conversation history, Caveman system
instructions, exact tool/reference excerpts, and hash-verified original recovery.
Explicit model capability, per-redirect download grants, offline engine launch,
Host/Origin checks, text-only inputs, admission-before-preprocessing, bounded
HTTP/IPC, descendant RSS supervision, cancellation and no replay are covered.

Linux verification includes fresh NativeAOT publication, Rust unit/ABI/store
tests, full Python regression tests, fake-engine crash/timeout/idle/cancellation
tests, and real pinned 1.2 MB GGUF chat/embeddings/CLI integration.
[Model benchmark](benchmarks/linux-model-preview.json) records three fresh model
processes and nine successful requests: median startup 51.26 ms, median request
8.77 ms for 46 prompt and 16 generated tokens. Those tiny-fixture CPU results
verify plumbing, not useful-model speed or quality; disk cache was uncontrolled.

Mac/GPU model execution remains unverified locally. Windows JSC is the accepted
tracked blocker. External resource monitoring is sampled, and model workers
are not a complete network/OS sandbox. HF selection/grants/integrity have focused
tests; an end-to-end production HF download was not performed. No production
model or signing key is included. Local model usage and constraints are documented
in [model hosting](model-hosting.md).

## CLI parsing and current core benchmarks — 2026-10-11

The handwritten token parser and help text have been replaced by exact-pinned
System.CommandLine 2.0.12 command/option/argument declarations and generated help.
Spectre.Console 0.57.2 continues to render human output. NativeAOT publication
with warnings treated as errors passed; no trimming warnings were suppressed.
The existing generated Request and permission/isolation validation remain the
execution contract. Model commands retain their parser surface, but no model
hosting or inference was changed or benchmarked in this pass.

Values accept either `--option=value` or `--option value`. Each `--allow-*`
occurrence requires one explicit nonempty value; a following option cannot
become a grant. Response-file expansion and directive actions are disabled.
Guest argv after `--` stays literal. Paths beginning with `-` use `./`. Tool-only
options appear only on their tool commands, and `--no-cache` overrides
`--cache-dir` regardless of flag order. Help can be requested without an entry
or execution permissions. Version aliases and stable JSON diagnostics remain
covered.

Core/frontend/tooling acceptance plus model parser-only compatibility: **81
tests passed**. Model process/inference suites were intentionally excluded.
Generated contract drift checks passed. The Rust engine/worker was unchanged.

[Current raw report](benchmarks/linux-core-preview.json) measures 100 samples
per mode on Linux x86_64 with protocol v3 and records exact CLI/worker hashes.
Every workload produces `42`, with zero transform/graph hits for empty caches
and two of each for populated caches. Empty/populated cache order alternates.

| Mode | Median | p95 |
| --- | ---: | ---: |
| CLI capabilities (launch-to-exit) | 2.58 ms | 2.83 ms |
| CLI parse + dry-run JSON (no worker) | 2.80 ms | 3.23 ms |
| Fresh JSC worker, empty transform/graph caches | 9.62 ms | 10.17 ms |
| Fresh JSC worker, populated transform/graph caches | 8.90 ms | 9.78 ms |

The CLI parsing number includes process launch and serialization, not just CPU
time inside the parser. Worker measurements include trace instrumentation; OS
page caches are uncontrolled. Populated caches do not retain a worker or JSC
context. Median context initialization was 1.87/1.87 ms (empty/populated),
and TS transforms 0.52/0.00 ms. These phases overlap with enclosing timings and
must not be added together. No model process, snapshot, engine bytecode cache,
daemon, or cross-runtime speed claim is involved. The older linux-mvp.json remains historical. The current core preview archive
is verified separately by the installed-core gate below.


## Core release gates — 2026-10-11

The final NativeAOT CLI and optimized Rust artifacts pass **88 Python core,
frontend, tooling and shared-parser tests**, with `OGMA_REQUIRE_SANDBOX=1`.
No model engine/process/inference suite was run. Core Rust tests pass **20
checks**; the irreversible seccomp helper is intentionally ignored directly
and exercised by the passing subprocess test. Formatting, Clippy (warnings
denied), contract drift and NativeAOT publication pass. CI workflow YAML and
embedded signing Python parse successfully; actual remote workflow runs remain
unverified.

Linux/macOS now share descriptor-relative Unix file enforcement. Explicit
symlink grant aliases, including aliases inside the canonical directory, use
the issued handle; child symlinks remain denied. A directory renamed/replaced
after grant issuance stays anchored to its original handle. Linux regression
checks pass. Actual macOS filesystem/runtime behavior still needs its CI run.

The installed-core verifier authenticates before bounded safe extraction and
runs the package under a separate working directory, HOME and minimal
environment. It checks empty/populated TS/ESM caches, deny-by-default reads,
trusted ABI, actual async native test execution, checker/fmt/lint/bundle,
bundled execution, append-once timeout side effects and a subsequent usable
CLI invocation. [Installed evidence](benchmarks/linux-installed-core-preview.json)
identifies the archive by SHA-256. This is clean-install proof on the current
Linux host, not a clean OS image.

The signing workflow uses the selected `OGMA_ED25519_PRIVATE_KEY` secret plus
an independently provisioned `OGMA_ED25519_PUBLIC_KEY` public PEM repository
variable. Tags and requested signing fail without the private key; any signing
fails without the trusted public key. Signature verification happens before
extraction and execution. The disposable-key installed test report is explicitly
marked test-only; it does not establish a production identity.

Remaining external acceptance items:

- Execute Linux and macOS Intel/Apple Silicon workflows on the actual repository.
  Linux runners must support required namespaces; the gate fails if unavailable,
  and no workflow weakens host sandbox policy.
- Provision the approved private CI secret and independently trusted public PEM,
  then obtain a signed preview from that identity. No repository/CI credentials
  or production signing identity are available in this workspace.
- Verify a fresh OS installation if that stronger deployment claim is needed.
- Windows JSC worker remains the user-accepted blocker. Complete OS sandboxing,
  hard cgroup limits, full Node compatibility, native addons and stable LSP are
  outside the agreed preview scope. JSC snapshots/code caches remain unsupported
  by the maintained embedding API.

## MCP tools preview — 2026-10-12

`ogma mcp init` creates a new TypeScript server project without adopting or
overwriting an existing destination. `ogma mcp serve` hosts newline-delimited
MCP stdio through the NativeAOT frontend. Generated `Ogma.mcpTool` bindings use
offline Rust JSON Schema validation and fresh isolated JSC workers per
discovery/call, with fixed CLI grants, cancellation and no automatic replay.
Worker IPC is now v4; ABI remains v1. Match CLI/worker versions.

The final build passes **103 Python core acceptance tests**, including **15 MCP
tests**, with `OGMA_REQUIRE_SANDBOX=1`. MCP checks cover lifecycle, exact Unicode
and NUL text, scaffold type checking, permissions, malformed/oversized/truncated
input, active-request limits, cancellation, EOF, closed stdout, stderr logs/traces,
worker crashes, and append-once timeout effects followed by usable calls. Core
Rust tests pass **21 checks**; the one ignored irreversible seccomp helper is
exercised by the passing subprocess test. NativeAOT, Clippy with denied warnings,
Rust formatting and generated drift pass. No model process was started.

An independent client using the official `@modelcontextprotocol/sdk` **1.32.1**
passes initialize, list, exact-text call and ping against the generated scaffold.
The SDK was installed only in a temporary test directory, with no runtime
dependency added to Ogma. The installed-package verifier now also creates,
type-checks and calls the scaffold under a separate HOME, working directory and
minimal environment. Its archive digest is recorded in
[MCP installed evidence](benchmarks/linux-installed-core-mcp-preview.json).

Fresh 100-sample launch-to-exit measurements of the protocol-v4 core binaries,
including binary hashes and raw samples, are in
[MCP core startup evidence](benchmarks/linux-core-mcp-preview.json). Previous
benchmark/archive records remain historical. No model process, persistent JSC
context, OS cold-cache control or cross-runtime speed claim is involved.

This MCP preview is stdio, text-only and stateless between calls. HTTP hosting,
resources, prompts, sampling, progress and list-change notifications are absent.
The archive is unsigned with explicit verification opt-in; it establishes no
production release identity. macOS execution and Windows JSC support retain the
external limitations recorded above. See [MCP usage and boundaries](mcp.md).
