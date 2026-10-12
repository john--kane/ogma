#!/usr/bin/env python3
"""Authenticate, unpack privately, and verify the installed core runtime (no models)."""
import argparse
import asyncio
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import subprocess
import tarfile
import tempfile
from datetime import datetime, timezone
from verify_release import verify
from release import sha256


def extract(archive, destination, max_bytes=2 * 1024**3, max_files=100_000):
    """Extract regular files into a new private directory; never overwrite a path."""
    if destination.exists() or destination.is_symlink():
        raise ValueError('installation destination must not exist')
    destination.mkdir(mode=0o700)
    seen, roots, total = set(), set(), 0
    try:
        with tarfile.open(archive, 'r:gz') as source:
            for member in source:
                raw = member.name
                parts = raw.split('/')
                if (not raw or '\\' in raw or ':' in raw or len(raw) > 4096
                    or any(part in ('', '.', '..') for part in parts)
                    or PurePosixPath(raw).is_absolute() or len(parts) > 64
                    or raw in seen or not (member.isfile() or member.isdir())):
                    raise ValueError('unsafe or duplicate archive entry')
                seen.add(raw)
                roots.add(parts[0])
                total += member.size
                if len(roots) != 1 or len(seen) > max_files or member.size < 0 or total > max_bytes:
                    raise ValueError('archive root/count/size limit exceeded')
                target = destination.joinpath(*parts)
                if member.isdir():
                    if member.size != 0: raise ValueError('directory has content')
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                stream = source.extractfile(member)
                if stream is None: raise ValueError('missing file content')
                with stream, target.open('xb') as output:
                    remaining = member.size
                    while remaining:
                        block = stream.read(min(1024 * 1024, remaining))
                        if not block: raise ValueError('truncated archive member')
                        output.write(block)
                        remaining -= len(block)
                target.chmod(0o755 if member.mode & 0o111 else 0o644)
        if not roots: raise ValueError('empty archive')
        return destination / next(iter(roots))
    except Exception:
        shutil.rmtree(destination)
        raise


def check_layout(installed):
    library = 'libogma_runtime.dylib' if platform.system() == 'Darwin' else 'libogma_runtime.so'
    system = 'darwin' if platform.system() == 'Darwin' else 'linux'
    arch = 'arm64' if platform.machine() in ('arm64', 'aarch64') else 'x64'
    required = ['ogma', 'ogma-worker', library, 'contracts/ogma.json', 'types/ogma.d.ts', 'docs/mcp.md',
                'tooling/node', 'tooling/adapter.mjs', f'tooling/node_modules/@biomejs/cli-{system}-{arch}/biome']
    for name in required:
        path = installed / name
        if not path.is_file() or path.is_symlink(): raise ValueError(f'missing regular package file: {name}')
    for name in ['ogma', 'ogma-worker', 'tooling/node']:
        if not os.access(installed / name, os.X_OK): raise ValueError(f'package executable is not executable: {name}')


async def mcp_smoke(cli, entry, working, environment, version):
    process = await asyncio.create_subprocess_exec(cli, 'mcp', 'serve', str(entry),
        '--allow-read=' + str(entry.parent), '--no-cache', cwd=working, env=environment,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    async def send(method, identity=None, parameters=None):
        message = {'jsonrpc': '2.0', 'method': method}
        if identity is not None: message['id'] = identity
        if parameters is not None: message['params'] = parameters
        process.stdin.write(json.dumps(message).encode() + b'\n')
        await process.stdin.drain()
        if identity is None: return None
        line = await asyncio.wait_for(process.stdout.readline(), 30)
        response = json.loads(line)
        if response.get('id') != identity or 'result' not in response:
            raise ValueError(f'installed MCP request failed: {response}')
        return response['result']
    try:
        initialized = await send('initialize', 1, {'protocolVersion': version,
            'capabilities': {}, 'clientInfo': {'name': 'ogma-installed-smoke', 'version': '1'}})
        if initialized.get('protocolVersion') != version or 'tools' not in initialized.get('capabilities', {}):
            raise ValueError('installed MCP protocol/capability mismatch')
        await send('notifications/initialized')
        listed = await send('tools/list', 2)
        if [tool['name'] for tool in listed['tools']] != ['echo']:
            raise ValueError('installed MCP scaffold discovery failed')
        called = await send('tools/call', 3, {'name': 'echo', 'arguments': {'text': 'installed MCP'}})
        if called != {'content': [{'type': 'text', 'text': 'installed MCP'}], 'isError': False}:
            raise ValueError('installed MCP call returned unexpected content')
        process.stdin.close()
        if await asyncio.wait_for(process.wait(), 5) != 0:
            raise ValueError('installed MCP server failed on EOF')
    finally:
        if process.returncode is None: process.kill()
        await process.wait()


def smoke(installed, working):
    check_layout(installed)
    working.mkdir()
    home = working / 'home'
    home.mkdir()
    # No workspace, developer HOME, ambient loader controls, or Node installation.
    environment = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C', 'LC_ALL': 'C',
                   'TMPDIR': str(working), 'DOTNET_CLI_TELEMETRY_OPTOUT': '1'}
    cli = str(installed / 'ogma')
    checks = []
    def run(name, args, expected=None, error=None):
        completed = subprocess.run([cli, *map(str, args), '--json'], cwd=working, env=environment,
                                   capture_output=True, text=True, timeout=30)
        try: response = json.loads(completed.stdout)
        except ValueError as e: raise ValueError(f'{name}: invalid JSON: {completed.stderr}') from e
        if error:
            if completed.returncode == 0 or not any(d['code'] == error for d in response.get('diagnostics', [])):
                raise ValueError(f'{name}: expected {error}, got {response}')
        elif completed.returncode != 0 or response.get('status', 'ok') != 'ok':
            raise ValueError(f'{name}: {response}; stderr: {completed.stderr}')
        if expected is not None and [event['text'] for event in response.get('output', [])] != expected:
            raise ValueError(f'{name}: unexpected output')
        checks.append(name)
        return response
    capabilities = run('capabilities', ['capabilities'])
    contract = json.loads((installed / 'contracts/ogma.json').read_text())
    if capabilities['engine'] != 'JavaScriptCore' or capabilities['protocolVersion'] != contract['protocolVersion']:
        raise ValueError('installed engine/protocol disagrees with packaged contracts')
    project = working / 'project'
    project.mkdir()
    (project / 'dep.ts').write_text('export const answer: number = 42;\n')
    main = project / 'main.ts'
    main.write_text('import { answer } from "./dep.ts";\nawait Ogma.sleep(1);\nconsole.log(answer);\n')
    cache = working / 'cache'
    args = ['run', main, '--allow-read', project, '--cache-dir', cache, '--trace']
    for name, hits in [('ts-esm-empty-cache', 0), ('ts-esm-populated-cache', 2)]:
        response = run(name, args, ['42'])
        for phase in ['transformCacheHits', 'graphCacheHits']:
            if not any(t['name'] == phase and t['count'] == hits for t in response['trace']):
                raise ValueError(f'{name}: wrong {phase}')
    run('deny-read-default', ['run', main, '--no-cache'], error='OGMA_PERMISSION_DENIED')
    run('trusted-abi', ['eval', 'console.log("abi");', '--trusted-in-process', '--no-cache'], ['abi'])
    test = working / 'installed.test.ts'
    test.write_text('Ogma.test("installed", async () => { await Ogma.sleep(1); if (6 * 7 !== 42) throw new Error("bad"); });')
    run('native-tests', ['test', test, '--allow-read', test, '--no-cache'], ['ok installed'])
    run('checker', ['check', project])
    run('formatter', ['fmt', project])
    run('linter', ['lint', project])
    bundle = working / 'bundle.js'
    run('bundler', ['bundle', main, '--output', bundle])
    run('bundled-execution', ['run', bundle, '--allow-read', bundle, '--no-cache'], ['42'])
    marker = working / 'timeout-marker'
    marker.write_text('')
    source = f'await Ogma.writeTextFile({json.dumps(str(marker))}, await Ogma.readTextFile({json.dumps(str(marker))}) + "once"); while (true) {{}}'
    run('timeout-no-replay', ['eval', source, '--allow-read', marker, '--allow-write', marker,
                             '--timeout-ms=500', '--no-cache'], error='OGMA_TIMEOUT')
    if marker.read_text() != 'once':
        raise ValueError('timeout operation was replayed or never executed')
    run('supervisor-survives-timeout', ['eval', 'console.log(42);', '--no-cache'], ['42'])
    server = working / 'mcp-server'
    run('mcp-scaffold', ['mcp', 'init', server])
    run('mcp-scaffold-checker', ['check', server])
    asyncio.run(mcp_smoke(cli, server / 'server.ts', working, environment, contract['mcp']['protocolVersions'][0]))
    checks.append('mcp-stdio-initialize-list-call')
    return {'checks': checks, 'capabilities': capabilities, 'cleanEnvironment': True, 'modelExecution': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=Path)
    parser.add_argument('--public-key', type=Path)
    parser.add_argument('--allow-unsigned', action='store_true')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        with tempfile.TemporaryDirectory(prefix='ogma-installed-preview-') as folder:
            root = Path(folder)
            # Signature and digest authenticate the same bytes copied from the
            # open archive descriptor. No unverified pathname is reopened.
            artifact = verify(args.manifest, args.public_key, args.allow_unsigned, snapshot_directory=root)
            digest = sha256(artifact)
            installed = extract(artifact, root / 'install')
            evidence = smoke(installed, root / 'work')
        report = {'schemaVersion': 1, 'kind': 'ogma-installed-core-preview',
                  'verifiedAt': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
                  'signature': 'verified' if args.public_key else 'unsigned-explicit-opt-in',
                  'artifact': artifact.name, 'sha256': digest, **evidence}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(args.output)
    except (OSError, ValueError, subprocess.SubprocessError, tarfile.TarError) as error:
        parser.exit(1, f'installed preview verification failed: {error}\n')


if __name__ == '__main__': main()
