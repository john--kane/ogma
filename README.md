<p align="center">
  <a href="https://john--kane.github.io/ogma/"><img src="docs/assets/ogma-logo.png" alt="Ogma logo" width="220" height="220"></a>
</p>
<h1 align="center">Ogma</h1>

<p align="center">An alternative to Deno, built for AI workflows in JavaScript and TypeScript.</p>

<p align="center">
  <a href="docs/runtime-guide.md">Documentation</a>
  &nbsp;•&nbsp;
  <a href="https://john--kane.github.io/ogma/">Website</a>
  &nbsp;•&nbsp;
  <a href="https://github.com/john--kane/ogma/issues">Issues</a>
  &nbsp;•&nbsp;
  <a href="docs/post-mvp-roadmap.md">Roadmap</a>
</p>

## What is Ogma?

Ogma is an **AI-focused alternative to Deno**. Build AI workflows in JavaScript
and TypeScript, run local models, and expose your tools to AI clients through
MCP—all from one CLI.

Local GGUF model hosting, OpenAI-compatible endpoints, and model APIs for your
scripts sit alongside native TypeScript execution and explicit permissions.
Ogma also includes a test runner, formatter, linter, bundler, type checker,
and a package installer for a subset of pure-ESM npm packages.

```sh
ogma run main.ts --allow-read=.
ogma model chat /absolute/path/model.gguf --allow-model
ogma mcp init ./my-tools
```

A C# NativeAOT CLI manages commands and diagnostics. An isolated Rust worker
executes your code with JavaScriptCore.

Ogma is a **preview**. Linux x86_64 has been tested locally; macOS Intel and
Apple Silicon builds are configured in CI. The Windows runtime is blocked by
the JavaScriptCore binding. Full Node.js compatibility is outside the current preview.

## AI workflows

- **Run local models.** Host GGUF models through llama.cpp, chat from the
  terminal, or connect clients to local OpenAI-compatible chat and embeddings endpoints.
- **Use models from TypeScript.** Call `Ogma.modelAsk` and `Ogma.modelEmbed`
  from scripts connected to a running Ogma model service.
- **Build MCP tools.** Scaffold and serve TypeScript tool servers over stdio,
  with explicit permissions and a fresh isolated worker for each tool call.

Start a local model service:

```sh
ogma model run /absolute/path/model.gguf --allow-model --port=8080
```

Then call it from `ask.ts`:

```ts
const answer = await Ogma.modelAsk(
  "Explain this TypeScript error",
  "Type 'string' is not assignable to type 'number'.",
);
console.log(answer);
```

```sh
ogma run ask.ts --allow-read=. --allow-model --port=8080
```

See [local model hosting](docs/model-hosting.md) for model setup and
[MCP servers](docs/mcp.md) for connecting your tools to AI clients.

## Build and run

Install the latest preview with:

```sh
curl -fsSL https://john--kane.github.io/ogma/install.sh | bash
```

Or build from source with:

- .NET SDK **10.0.112**
- Rust with edition 2024 support
- Python **3.12+**
- Node **26.8.2** and npm
- CMake and a C++17 compiler for local model hosting
- Platform C/C++ build tools: Clang, zlib development headers, Make, and a C++
  compiler on Ubuntu; Xcode command-line tools on macOS

```sh
git clone https://github.com/john--kane/ogma.git
cd ogma
python3 tools/build.py
./ogma run tests/fixtures/hello.ts --allow-read=tests/fixtures
```

The first build downloads pinned engine sources and tool packages, and can
take several minutes and several GB. Keep the complete `artifacts/`
directory together. The full build includes the local model engine; supply your
own GGUF model. Use `--core-only` for the runtime and MCP tools without model hosting,
in a fresh checkout/output to exclude model artifacts from earlier builds.

File, environment, and network access are denied by default. Grant only what
your script needs, including read access to its entry module:

```sh
ogma run main.ts --allow-read=. --allow-net=example.com:443
ogma run main.ts --allow-read=. --allow-write=./output -- --name demo
```

Write grant directories must already exist. Use `ogma <command> --help` for
options, or see the [runtime guide](docs/runtime-guide.md#build-and-run) for
permissions, resource limits, and build details.

## Built-in tools

```sh
ogma test tests/fixtures/hello.ts --allow-read=tests/fixtures
ogma fmt src                 # check formatting
ogma fmt src --write         # apply formatting
ogma lint src
ogma bundle main.ts --output=dist/main.js
ogma check .
ogma install .
ogma install . --locked      # use the verified offline store
```

Formatting and linting use Biome; bundling uses Rolldown; type checking uses
TypeScript. The installer writes `ogma.lock.json` and never runs package scripts.
See [tooling and package limits](docs/runtime-guide.md#built-in-tools).

## Quick links

- [Local model hosting](docs/model-hosting.md)
- [MCP servers](docs/mcp.md)
- [Runtime APIs and contracts](docs/runtime-guide.md#runtime-and-contracts)
- [REPL, JSON-RPC, and language server](docs/runtime-guide.md#repl-rpc-and-language-server)
- [Permissions and security boundaries](docs/runtime-guide.md#security-boundaries)
- [Sandbox and preview releases](docs/sandbox-and-releases.md)
- [Verification and benchmarks](docs/runtime-guide.md#verification-measurements-and-releases)
- [Project website](site/README.md)
- [Roadmap](docs/post-mvp-roadmap.md)

## Contributing

Start with the [product goals](PRODUCT.md) and [architecture](DESIGN.md).
Build the core preview, then run the contract and runtime checks:

```sh
python3 tools/generate_contracts.py --check
python3 tools/test_core.py
```

See the [runtime guide](docs/runtime-guide.md#verification-measurements-and-releases)
for the full verification commands and the [verification record](docs/mvp-verification.md)
for tested behavior and current limitations.
