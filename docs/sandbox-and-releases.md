# Sandbox and preview packaging

`--sandbox=linux` requests an optional prototype for isolated `run`, `eval`,
`test` and `repl` commands. It requires Linux and `/usr/bin/bwrap`. The launcher
probes user/PID/mount/network namespace support, then starts the worker with
bubblewrap's private root, PID 1, network namespace and dropped capabilities.
Failure returns `OGMA_SANDBOX_UNAVAILABLE`; execution never falls back to the
ordinary worker. Trusted execution and tooling/install commands cannot use it.

The filesystem view contains read-only platform runtime directories (`/usr`,
`/lib`, `/lib64`, `/bin`, `/sbin`, loader cache and timezone data), the runtime
distribution, explicit read/write grants, the writable dedicated cache, a
private `/tmp`, `/dev` and `/proc`. The host's home and other files are absent
unless granted. System runtime data remains visible independently of guest API
grants. Root, `/proc` and `/dev` grants are rejected. Granting broad directories
deliberately broadens the namespace view.

The supervisor samples aggregate resident memory for bubblewrap and its visible
descendants so the wrapper's small memory usage cannot hide the guest worker.
This remains sampled enforcement, without a cgroup hard cap.

Before JSC initializes, Rust requires the worker to be PID 1 with a tmpfs root
and installs a seccomp filter across all threads. The filter blocks socket
creation, process execution/creation, namespace/mount/privilege operations,
ptrace, cross-process memory, io_uring and related privileged facilities. JSC
JIT mappings, filesystem operations inside the namespace and thread creation
remain available. Anonymous Unix socket pairs remain available for Tokio's
signal driver; socket creation and connections to endpoints are blocked.
`clone3` returns ENOSYS so libc can create threads using the
filtered `clone` call. This is a syscall deny-list prototype, not a complete
hard sandbox, syscall allow-list or cgroup resource boundary.

The tool sandbox denies namespace creation, but authorized host execution on
this Linux machine supports bubblewrap. Required combined tests have run real
TS/ESM/JSC and Tokio under namespaces plus seccomp, verified file grants and
cache reuse, denied child symlinks, and checked timeout cleanup without replay.
Separate seccomp subprocess tests also verify that threads work while sockets,
process clone and exec fail. CI sets `OGMA_REQUIRE_SANDBOX=1`: unavailable
namespaces fail the gate rather than count as success. A runner must support
this confinement; no workflow disables host AppArmor or namespace policy.

## Reproducible archives and signatures

Build first, then package the resulting distribution:

```sh
python3 tools/release.py --input artifacts --output dist --name ogma-linux-x86_64-preview
python3 tools/verify_release.py dist/ogma-linux-x86_64-preview.manifest.json --allow-unsigned
```

The archive sorts entries, normalizes ownership and modes, and fixes tar/gzip
timestamps to `SOURCE_DATE_EPOCH` (default zero). Reproducibility concerns the
same input files, not independently compiled binaries. Symlinks and special
files are rejected; the build must copy tool dependencies as regular files.
The adjacent JSON manifest records the archive's byte length and SHA-256.

For a signed preview, supply your own unencrypted Ed25519 PEM key stored outside
the packaged input. The tool never creates a production key:

```sh
python3 tools/release.py --input artifacts --output dist --name ogma-linux-x86_64-preview --sign-key /private/release-key.pem
python3 tools/verify_release.py dist/ogma-linux-x86_64-preview.manifest.json --public-key /trusted/release-public.pem
```

OpenSSL signs the exact manifest bytes; verification authenticates those bytes
with the independently trusted public key before checking the archive digest.
Do not obtain your trust key from an untrusted archive. Unsigned manifests have
no authenticity guarantee and require explicit opt-in. Tests use disposable
keys and verify manifest/archive tamper rejection.

The installed-core gate authenticates metadata first, copies and hashes archive
bytes from the same open descriptor, then extracts into a private temporary
directory. It rejects traversal, links, special files, duplicates and excessive
size/count/depth, and strips privileged modes. The CLI runs from a separate
working directory and HOME with no developer loader controls or Node on PATH.
It verifies TS/ESM/cache states, permissions, trusted ABI, native tests, all tool
adapters, bundled execution and no-replay timeout cleanup. This proves a clean
installation/environment on the test host, not a fresh OS image:

```sh
python3 tools/verify_preview.py dist/ogma-linux-x86_64-preview.manifest.json --public-key /trusted/release-public.pem --output dist/installed-core-preview.json
```

CI tests Linux and macOS Intel/Apple Silicon builds and creates unsigned
archives. The separate preview packaging workflow runs on version tags or
manual dispatch, signs only when the caller supplies the
`OGMA_ED25519_PRIVATE_KEY` secret, and labels artifacts as signed or unsigned.
Version tags and manual signing requests fail if the secret is absent. Signing
also requires an independently configured Ed25519 public PEM in the repository
variable `OGMA_ED25519_PUBLIC_KEY`; verification does not derive its trust key
from the signer. A wrong public key fails before extraction/execution. Core CI
uses `tools/build.py --core-only` and `tools/test_core.py`, avoiding model-engine
preparation and inference. Installed verification and raw startup evidence are
uploaded alongside archives. Workflows upload CI
artifacts; they do not publish a release or establish a production identity.
