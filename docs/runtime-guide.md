# Ogma runtime guide

Ogma is a JS/TS runtime preview built as **C# NativeAOT → isolated Rust worker → JavaScriptCore**. JSC replaces the original V8 design by user choice. C# owns commands, diagnostics, supervision, REPL, JSON-RPC and the language-server frontend. Rust owns guest execution, module loading, permissions, Tokio I/O, caches and tool adapters; guest operations never pass through C#.

The MVP implementation targets Linux x86_64 and macOS x86_64/arm64. Linux has been built and tested locally. macOS CI is configured, but execution has not been verified in this workspace. Windows has a NativeAOT/contracts CI job; the maintained JSC binding's Windows worker remains a tracked blocker.

## Build and run

Install .NET SDK 10.0.112, a Rust toolchain supporting edition 2024, Python 3.12+, Node **26.8.2**, npm and the platform's C/C++ build tools. Ubuntu NativeAOT needs Clang and zlib development headers; ICU preparation also needs Make and a C++ compiler. macOS needs Xcode command-line tools. The first build downloads checksum-pinned JSC/ICU sources and integrity-locked tool packages; engine preparation can take several minutes and several GB.

```sh
python3 tools/build.py --core-only
./ogma capabilities --json
./ogma run tests/fixtures/hello.ts --allow-read=tests/fixtures
./ogma eval 'const answer: number = 42; await Ogma.sleep(1); console.log(answer);'
./ogma test tests/fixtures/hello.ts --allow-read=tests/fixtures --json
./ogma check tests/fixtures/hello.ts
./ogma repl
```

Keep the complete `artifacts/` distribution together, including `tooling/`, `types/`, the worker and shared library. Core-only builds skip llama.cpp preparation; use `tools/build.py` for the existing model-inclusive distribution. Build into a fresh checkout/output when a core-only package must exclude older model artifacts. `tools/build.py --debug-worker` retains the NativeAOT CLI while building Rust without release optimization.

Entry modules require a read grant. Repeat explicit grants using `--allow-read=path` or `--allow-read path` (and the corresponding other grant flags). Write roots must already exist; files can be created beneath granted directories. Script arguments follow `--` and are passed literally; response-file expansion and CLI directives are disabled. Paths beginning with `-` should use `./`. `--no-cache` takes precedence over `--cache-dir`. Command-specific help is available with `ogma run --help`, `ogma bundle --help`, etc.

```sh
ogma run main.ts --allow-read=. --allow-write=./output --allow-env=HOME -- --name demo
ogma run main.ts --allow-read=. --allow-net=example.com:443 --json --trace
ogma run main.ts --allow-read=. --dry-run
```

`--dry-run` prints the generated request without execution; `--explain` describes its security boundary. Defaults are a 30-second deadline, 256 MiB sampled resident memory and 1 MiB guest output. `--timeout-ms`, `--memory-mb` and `--max-output-bytes` change these bounds. Ctrl+C terminates isolated execution without replay.

Command parsing and generated help use pinned **System.CommandLine 2.0.12**; human terminal output uses **Spectre.Console 0.57.2**. The CLI remains NativeAOT with warnings treated as errors.

## Runtime and contracts

- Native local `.js`, `.mjs`, `.ts`, `.mts` ESM, dynamic import and top-level await. TS runs through pinned `deno_ast=0.49.0`/SWC with separate source maps; full type checking stays off the execution path.
- Generated guest APIs: `Ogma.args`, `Ogma.env`, async `Ogma.readTextFile`, `Ogma.writeTextFile`, `Ogma.sleep`, `Ogma.fetchText`, `Ogma.test` and bounded console output. Tests run sequentially and unresolved tests fail.
- SHA-256-verified, atomically published transform/content cache and persistent graph metadata. Every source is reopened under current grants and hashed before reuse. Local URL edges can be reused; npm manifests are always resolved under current grants. Changed sources invalidate their hints; corrupt entries become misses. `--no-cache` disables persistence.
- Framed worker protocol **v4**: 4-byte little-endian length followed by UTF-8 JSON, maximum 8 MiB. Stdout is reserved for frames; guest output is returned as events. Ordinary requests execute once; REPL explicitly retains one worker/context.
- Trusted C ABI **v1**: numeric opaque handles, caller-owned response buffers, structured statuses, panic containment and generated source-based `LibraryImport`. `--trusted-in-process` opts into this boundary; sandbox and persistent-session requests are refused there.
- Bounded structured trace events expose phase names, elapsed microseconds and numeric counts for module, transform and graph cache activity.

`contracts/ogma.json` is the source of truth for C#/Rust models, source-generated JSON metadata, C ABI declarations, protocol schema, guest bootstrap and JS/TS declarations. Run `python3 tools/generate_contracts.py`; `--check` verifies drift. Protocol v1–v3 schemas remain available; the current worker accepts v4. `types/ogma.d.ts` describes the guest API; `types/protocol.d.ts` and `types/protocol.js` describe IPC/status contracts.

Engine pins are `rust_jsc=0.5.0` / `rust_jsc_sys=0.4.2`; archive digests are in `contracts/engines.lock.json`. Linux preparation rebuilds pinned ICU 70.1 with PIC for the shared ABI library. Ogma adds no C++ binding; its watchdog uses one private JSC C entry point from the pinned engine. Supported bindings expose no verified startup snapshot or persistent engine bytecode cache. Safe builtins initialize per context. There is no warm daemon or claimed engine-code-cache speedup.

## Built-in tools

See the [post-MVP web and AI roadmap](post-mvp-roadmap.md) for proposed
extensions and selected ideas from Bun's command surface.

```sh
ogma fmt src                 # check formatting
ogma fmt src --write
ogma lint src
ogma bundle main.ts --output=dist/main.js
ogma check .
ogma install .
ogma install . --locked      # verified offline store only
```

Rust launches pinned **Biome 2.5.15** directly for fmt/lint. A private **Node 26.8.2** helper hosts **Rolldown 1.2.13** and **TypeScript 6.0.3** for bundling/checking. Node runs tooling only; guest execution remains JSC. Dependencies and integrity hashes are pinned in `tools/runtime/package-lock.json`, installed with lifecycle scripts disabled. Bundle output is ESM with a source map. Tool commands authorize the selected project operation; they are not guest APIs. Direct `check` grants its selected project read access; RPC/LSP checking stays within server read grants. Helpers receive a cleared environment, bounded output and descendant memory/deadline supervision.

Install supports a deliberate pure-ESM npm subset: exact, caret and tilde full-semver ranges, scoped names and nested conflicting dependency versions. It writes deterministic `ogma.lock.json`, uses a SHA-512 SRI content-addressed tarball store (default `.ogma/store`) and atomically replaces `node_modules`. `--locked` verifies existing tarballs and never accesses the registry. `--registry` and `--store` select explicit alternatives. Installation never runs package scripts or package code.

Native files, CommonJS, lifecycle scripts, peers, optional/bundled dependencies, workspaces and dependency cycles are rejected. Archive extraction rejects traversal, links, duplicate entries and special files, with graph/file/count/depth bounds. A durable `.ogma-install-journal.json` guards publication of modules and lockfile. An interrupted publication blocks both installation and installed-package imports, preserves staging/backups and requires explicit restoration of a matching pair before removing the journal. It never silently resumes or replays installation. This is detectable recovery, not a transactional filesystem spanning two destinations.

## REPL, RPC and language server

The REPL retains a single isolated JSC context and accepts one-line TS submissions, await, imports and persistent declarations. Guest errors can continue; crashes, cancellation and terminal resource failures end the session without replay. Top-level declarations become mutable global properties, so `const`/TDZ semantics are limited; REPL exports are unsupported.

`ogma rpc [permission flags]` serves bounded Content-Length JSON-RPC 2.0 framing over stdin/stdout. Methods are `eval`, `run`, `check`, `capabilities`, with `$/cancelRequest`. Caller parameters cannot elevate the fixed server permission grants.

`ogma lsp --allow-read=<project>` is a preview C# frontend for initialize/shutdown/exit, full document synchronization and versioned TypeScript diagnostics, including unsaved text. It bounds document sizes/counts and checks at most four documents concurrently; superseded diagnostics are suppressed. It provides diagnostics, not a stable or complete language server.

## Security boundaries

Guest fs, env and network are denied by default. `Ogma.fetchText` supports HTTP/HTTPS, checks exact host or host:port grants before every redirect, disables proxy inheritance and bounds responses to 16 MiB UTF-8. A host grant without a port permits that host's ports. DNS is resolved by the host stack; grants govern URL hosts, not destination IP ranges. Subprocess/native-library flags are reserved; these guest APIs, native addons and full Node compatibility are outside the MVP.

Linux and macOS file grants share Unix descriptor traversal (`openat`, `O_NOFOLLOW`) and regular-file validation, rejecting child symlink components. Explicit grant aliases remain anchored to the granted handle even when their paths are renamed; Linux regression tests pass, while actual macOS execution remains a CI gate. Dedicated cache paths are reserved against guest APIs even with broad grants. Cache hashes detect corruption, not malicious modification by another process with the same user identity; use a private cache.

Default isolation is a separate process. Environment values are inherited only when explicitly granted; loader/JSC control variables are reserved. The supervisor validates replies, enforces deadlines, samples RSS and kills process trees without replay. Tool/sandbox RSS includes descendants created by all threads. Sampling can overshoot limits; no cgroup hard cap is claimed. Unix workers set CPU/core/descriptor limits, and Linux sets `no_new_privs`. Persistent REPL uses per-evaluation watchdogs rather than a process-lifetime CPU deadline.

`--sandbox=linux` opts into a **fail-closed Linux namespace/seccomp prototype**, requiring `/usr/bin/bwrap`. It isolates filesystem/PID/network views and blocks process execution, network sockets and privileged syscalls while permitting JSC/Tokio threads. Combined namespaces/seccomp/JSC/Tokio execution has passed on this Linux host through authorized host execution, including filesystem grants, cache reuse and no-replay timeout cleanup. The tool sandbox itself denies namespace creation; CI requires a real successful combined run. It is a syscall deny-list prototype, not complete OS sandboxing. See [sandbox and release details](sandbox-and-releases.md).

Trusted ABI execution shares the frontend's native process: engine fatal errors/aborts can terminate it, and external cancellation/memory limits are unavailable. Rust unwinding panics are contained. Cancellation cannot roll back completed side effects; output can be lost if a worker is killed before replying.

## Verification, measurements and releases

```sh
python3 tools/generate_contracts.py --check
RUST_JSC_CUSTOM_BUILD_PATH="$(python3 tools/prepare_jsc.py | tail -1)" cargo test --locked -p ogma-runtime
python3 -m unittest discover -s tests -v
python3 tools/test_core.py
python3 tools/benchmark.py --samples=30 --output=artifacts/startup.json
python3 tools/release.py --input artifacts --output dist --name ogma-linux-x86_64-preview
python3 tools/verify_preview.py dist/ogma-linux-x86_64-preview.manifest.json --allow-unsigned --output=dist/installed-core-preview.json
```

Tests exercise TS/ESM/maps, real tool adapters, offline/integrity/hostile npm inputs and interrupted publication, network permission/redirect/body limits, REPL/RPC/LSP, cache corruption/source changes/grant changes, native string/ABI boundaries, cancellation, descendant memory and no-replay crashes. Release tests use disposable Ed25519 keys and reject tampering, key mismatch, downgrade and manifest substitution.

Startup reports record raw samples, distribution statistics, binary hashes and environment. Measurements cover launch-to-exit CLI time and fresh workers with empty/populated transform and graph caches. OS page caches are uncontrolled. Current core measurements are in `docs/benchmarks/linux-core-preview.json`; the earlier `linux-mvp.json` records historical protocol-v2 binaries. The current report includes parsing-only dry-run launch time and traced worker phases, with verified transform/graph cache state. No model hosting is included. Rerun on target hardware before drawing conclusions.

CI builds Linux and macOS Intel/Apple Silicon previews, plus Windows contracts/frontend. Deterministic archives normalize metadata and timestamps for identical input files. The release workflow uses the user-selected **`OGMA_ED25519_PRIVATE_KEY` CI secret** for signing, and authenticates against an independently configured **`OGMA_ED25519_PUBLIC_KEY` repository variable** before installed-package smoke verification and upload. Version tags and explicit signing requests require the private key; any signed build requires the trusted public key. Core CI skips model preparation and inference. No production signing key is available in this workspace, and no official signed release has been published here; remote secret provisioning remains unverified. `ogma capabilities --json` distinguishes signing support from this unsigned build and reports engine/platform exclusions explicitly.

Local GGUF model hosting is available through `ogma model run`, `model chat`, and
`model ask`, gated by `--allow-model`. Rust owns the pinned llama.cpp process,
loopback OpenAI endpoint, context excerpts, and inference. See
[model hosting](model-hosting.md) for examples, model download permissions,
JS APIs, recovery, and measured limitations.


## MCP servers

Create a TypeScript tools server and connect it to an MCP stdio client:

```sh
ogma mcp init ./my-server
ogma mcp serve ./my-server/server.ts --allow-read=./my-server
```

Tools register through generated `Ogma.mcpTool` bindings and return text. Each
discovery/call uses a fresh supervised worker with fixed explicit CLI grants;
client arguments cannot elevate permissions. Guest logs go to stderr. See
[MCP server documentation](mcp.md) for client configuration, cancellation,
resource controls and the stateless stdio preview limits.

## Project website

The dependency-free landing page lives in [site/](../site/README.md), with a
GitHub Pages workflow in `.github/workflows/pages.yml`. Build and preview it:

```sh
python3 tools/build_site.py
python3 -m http.server 4173 --directory dist/site --bind 127.0.0.1
```

Open http://127.0.0.1:4173. Select **GitHub Actions** in the repository's
**Settings → Pages** to deploy from its default branch. Benchmark numbers are
built from the checked-in core startup report; the website build does not
compile or change the runtime.
