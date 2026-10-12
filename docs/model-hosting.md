# Local model hosting

Ogma hosts a local GGUF through its C# command frontend, isolated Rust worker,
and a pinned llama.cpp server. Python prepares binaries and measures HTTP
requests; it does not run inference. NativeAOT remains enabled. Commands use System.CommandLine parsing and
Spectre.Console terminal output.

Build the full distribution with `python3 tools/build.py`. This packages
`artifacts/model/llama-server`, the source pin, build metadata, and licenses.
The first build downloads the exact source archive in
`contracts/llama.lock.json`, checks SHA256 before extraction, and compiles the
Release `llama-server` target from `tools/server`. Later builds reuse a cache
only when the pin, options, and binary hash match. CMake and a C++17 compiler
are required. `--llama-jobs=2` limits concurrency; the default is at most four.
To prepare just the server, run
`python3 tools/prepare_llama.py --jobs 2 --package artifacts/model`.

The bundled Linux build is a CPU preview: CUDA and Vulkan are disabled;
`GGML_NATIVE=OFF` avoids tuning for the build host. Upstream's x86 defaults
still require AVX, AVX2, FMA, F16C, and BMI2. This is not a universal x86 binary.
Engine libraries are linked statically; Linux system C/C++ and OpenMP runtime
libraries are still required. macOS preparation enables Metal and embeds its
shader library, but macOS and GPU execution have not been verified here. The
default `--gpu-layers=0` keeps inference on CPU; GPU offload requires an explicit
layer count and a compatible engine. Linux GPU users can supply a trusted
external executable with `--llama-server=/absolute/path/llama-server`; its
backend and build compatibility are their responsibility. GGUF quantization
support comes from the pinned engine, without a conversion step in Ogma.

Run a model with:

```sh
artifacts/ogma model run /absolute/path/model.gguf --allow-model --port=8080
```

The public service binds only `127.0.0.1`; its OpenAI-compatible
`/v1/chat/completions` and `/v1/embeddings` routes are owned by the Rust proxy.
The backend has a separate ephemeral port and an API key. Model permission
does not grant remote network access. Rust resolves model files and owns model
downloads; local inference launches llama-server with a local path and
`--offline`. This source release removed CURL, and offers no compile switch to
remove the HF downloader. Preparation disables OpenSSL and prebuilt UI
downloads; runtime offline mode prevents backend model downloads. Downloading
a model through Ogma additionally requires the relevant `--allow-net` grant.

The pinned release's help describes `--embedding` as embedding-only, but a
real CPU probe of its generative tiny Llama fixture verified simultaneous chat
and OpenAI embeddings with `--embedding --pooling mean`. Without the embedding
flag, embeddings return HTTP 501. Two fixture inputs produced two vectors of
64 floats. This verifies protocol behavior for that architecture; it makes no
embedding quality claim or guarantee for other GGUF architectures. Engine
errors must remain visible to callers.

Benchmark through the shipped frontend and loopback endpoint:

```sh
python3 tools/benchmark_model.py /absolute/path/model.gguf \
  --runs 3 --requests 3 --output .build/model-benchmark.json
```

Each run starts a fresh `ogma model run` process, waits for JSON `/health`
readiness, sends nonstreaming chat requests, then shuts down the process.
Reports include every startup and request sample, median wall times, actual
prompt/generated token usage, engine token timings when provided, model and
server hashes, build options, and CPU flags. Context defaults to 4096, GPU
layers to zero, and parallel slots to one. Prompt caching is disabled in
requests. Operating-system file caching is not controlled, so these are fresh
process timings, not guaranteed cold disk-load timings.

The locked `stories260K.gguf` fixture is only about 1.2 MB. It establishes real
model loading, generation, embedding shape, authentication, and request
accounting. Its speed and output do not predict useful-model quality or
throughput. It is downloaded only for verification and is not packaged as an
end-user model.

## Chat, ask, and JS

```sh
# Connect to the running local service:
artifacts/ogma model chat --allow-model --port=8080
artifacts/ogma model ask 'Explain this error' --allow-model --caveman
# Load a transient model, answer once, then reap the server:
artifacts/ogma model ask 'Explain this error' --model=/absolute/path/model.gguf --allow-model
# Load a model for this interactive session:
artifacts/ogma model chat /absolute/path/model.gguf --allow-model --caveman-ultra
```

`--ctx` is context per parallel slot (default 4096); `--parallel` defaults to 1.
The total context is limited to 1048576 tokens. Chat keeps history in Rust and
fails explicitly at its bounded history limit; it never silently discards turns.
Ctrl+C, EOF in chat, request timeout, memory limit, or backend failure ends the
owned session and reaps its processes. No inference is replayed after failure.
`model run` stays alive until cancellation, including with redirected/closed stdin.
The frontend continuously samples descendant RSS; default model budget is
4096 MiB with a 120-second per-operation deadline. These sampled limits are
not a hard memory quota or complete OS sandbox.

Within a guest script started with `--allow-model --port=8080`:

```ts
console.log(await Ogma.modelAsk("Explain the error", "optional reference text"));
const vector: number[] = await Ogma.modelEmbed("text to embed");
```

JS APIs only connect to an existing Ogma service. They cannot select a model or
executable. Hosting flags require a `model` command. `types/ogma.d.ts` is generated
from the same source as C#/Rust contracts. Worker protocol is v3; ABI remains v1.
Trusted in-process execution and the Linux guest sandbox prototype reject model
capabilities. The service is a text-only preview: remote media inputs are rejected.
OpenAI streaming chat is supported; embeddings use the model's mean pooling.
The public endpoint grants access to local clients without an API key; backend
access uses a random key. Do not expose the public port through a network proxy.
Every route checks loopback Host authority; browser Origin requests are denied.

## Context and model recovery

`--caveman` and `--caveman-ultra` add short system instructions for compact output.
User message content is preserved exactly. OpenAI clients can set
`ogma_caveman` to `compact`, `ultra`, or the empty string per request. These modes
request an output style; they do not guarantee model compliance.

Tool-role text over 16 KiB and explicit reference context are reduced to bounded,
deterministic excerpts of exact complete head/tail/error lines. Originals are
stored under `<model-store>/contexts/<SHA256>` and verified on recovery via
`GET /ogma/context/<SHA256>`. OpenAI clients pass explicit context in
`ogma_context`; CLI `--context=/path/file` additionally requires
`--allow-read=/path/file`. Context is sent as a separate reference message.
Ordinary user prompts are never compressed. HTTP request/response sizes are
bounded to 8 MiB, and busy inference requests return 429 before preprocessing.

Local regular GGUF files are copied into `<model-store>/blobs/<SHA256>.gguf`;
default store is `.ogma/models` in the working directory. Atomic, private store
writes reject symlinks and verify content on reuse. No production model is shipped.
HF ids accept `owner/repo` or `hf:owner/repo::filename.gguf`. Automatic selection
requires a unique Q4_K_M or a single GGUF; ambiguous and split models fail.
Downloads pin the resolved commit and verify the file digest. Cached pins can be
used offline. Initial downloads require exact host/port network grants for HF and
each CDN redirect (for example `--allow-net=huggingface.co:443`); missing grants
fail explicitly and must be added for the actual redirected host. No ambient
proxy, token, or remote network permission is inherited by local inference.
