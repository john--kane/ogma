#!/usr/bin/env python3
"""Build the verified, pinned local llama-server. No Python inference dependency."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
LOCK_PATH = ROOT / 'contracts/llama.lock.json'
LOCK = json.loads(LOCK_PATH.read_text())


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def download(item, destination):
    if destination.exists() and sha256(destination) == item['sha256']:
        return destination
    temporary = destination.with_suffix('.download')
    print('Downloading pinned artifact:', item['url'], flush=True)
    try:
        with urllib.request.urlopen(item['url'], timeout=120) as response, temporary.open('wb') as output:
            shutil.copyfileobj(response, output)
        if sha256(temporary) != item['sha256']:
            raise RuntimeError('llama.cpp archive SHA256 mismatch')
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def prepare(jobs=None):
    system = platform.system()
    if system not in ('Linux', 'Darwin'):
        raise RuntimeError('Pinned llama.cpp preparation supports Linux and macOS; supply a compatible external server on other platforms')
    jobs = min(os.cpu_count() or 2, 4) if jobs is None else jobs
    if not 1 <= jobs <= 16:
        raise ValueError('llama.cpp build jobs must be between 1 and 16')
    base = ROOT / '.build/llama'
    base.mkdir(parents=True, exist_ok=True)
    source = base / ('llama.cpp-' + LOCK['commit'])
    build = base / (system.lower() + '-' + platform.machine())
    flags = [
        '-DCMAKE_BUILD_TYPE=Release', '-DBUILD_SHARED_LIBS=OFF', '-DGGML_NATIVE=OFF', '-DGGML_CCACHE=OFF',
        '-DLLAMA_BUILD_TESTS=OFF', '-DLLAMA_BUILD_EXAMPLES=OFF', '-DLLAMA_BUILD_TOOLS=ON',
        '-DLLAMA_BUILD_SERVER=ON', '-DLLAMA_BUILD_APP=OFF', '-DLLAMA_BUILD_UI=OFF',
        '-DLLAMA_USE_PREBUILT_UI=OFF', '-DLLAMA_OPENSSL=OFF', '-DLLAMA_BUILD_BORINGSSL=OFF',
        '-DGGML_CUDA=OFF', '-DGGML_VULKAN=OFF', '-DGGML_METAL=' + ('ON' if system == 'Darwin' else 'OFF'),
        '-DGGML_METAL_EMBED_LIBRARY=ON',
    ]
    # This release removed CURL. HF downloads have no build switch; the worker
    # must pass --offline and a resolved local -m path on every launch.
    fingerprint = hashlib.sha256(json.dumps({'lock': LOCK, 'flags': flags}, sort_keys=True).encode()).hexdigest()
    binary = build / 'bin/llama-server'
    marker = build / 'ogma-prepared.json'
    if binary.exists() and marker.exists():
        previous = json.loads(marker.read_text())
        if previous.get('fingerprint') == fingerprint and previous.get('binarySha256') == sha256(binary):
            return binary
    archive = download(LOCK['source'], base / 'source.tar.gz')
    if source.exists():
        shutil.rmtree(source)
    with tarfile.open(archive) as bundle:
        bundle.extractall(base, filter='data')
    build.mkdir(parents=True, exist_ok=True)
    print('Building pinned llama-server; log:', build / 'build.log', flush=True)
    with (build / 'build.log').open('w') as log:
        try:
            subprocess.run(['cmake', '-S', str(source), '-B', str(build), *flags], stdout=log, stderr=log, check=True)
            subprocess.run(['cmake', '--build', str(build), '--config', 'Release', '--target', 'llama-server', '-j', str(jobs)], stdout=log, stderr=log, check=True)
        except subprocess.CalledProcessError as error:
            raise RuntimeError(f'llama.cpp build failed; inspect {build / "build.log"}') from error
    metadata = {'fingerprint': fingerprint, 'commit': LOCK['commit'], 'binarySha256': sha256(binary),
                'platform': system, 'architecture': platform.machine(), 'cmakeFlags': flags}
    marker.write_text(json.dumps(metadata, indent=2) + '\n')
    return binary


def package(destination, jobs=None):
    binary = prepare(jobs)
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, destination / 'llama-server')
    shutil.copy2(binary.parent.parent / 'ogma-prepared.json', destination / 'build.json')
    shutil.copy2(LOCK_PATH, destination / 'llama.lock.json')
    source = ROOT / '.build/llama' / ('llama.cpp-' + LOCK['commit'])
    shutil.copy2(source / 'LICENSE', destination / 'LICENSE.llama.cpp')
    licenses = binary.parent.parent / 'licenses'
    if licenses.exists():
        shutil.copytree(licenses, destination / 'licenses', dirs_exist_ok=True)
    # Preserve third-party notices, including dependencies compiled into the server.
    notices = destination / 'third-party-licenses'
    for folder in ('vendor', 'ggml'):
        for notice in (source / folder).rglob('*'):
            if notice.is_file() and notice.name.upper().startswith(('LICENSE', 'COPYING')):
                target = notices / notice.relative_to(source)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(notice, target)
    return destination / 'llama-server'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jobs', type=int, default=None, help='Build concurrency (1..16; default at most 4)')
    parser.add_argument('--package', type=Path, help='Copy standalone server, pin, and licenses into this directory')
    parser.add_argument('--test-fixture', action='store_true', help='Download only the pinned tiny real GGUF verification fixture')
    args = parser.parse_args()
    if args.test_fixture:
        fixture = ROOT / '.build/llama-test/stories260K.gguf'
        fixture.parent.mkdir(parents=True, exist_ok=True)
        print(download(LOCK['testFixture'], fixture))
    else:
        print(package(args.package, args.jobs) if args.package else prepare(args.jobs))
