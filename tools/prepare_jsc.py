#!/usr/bin/env python3
"""Verify engine archives; rebuild the archive's ICU with PIC for the shared ABI."""
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
LOCK = json.loads((ROOT / 'contracts/engines.lock.json').read_text())

def download(item, dest):
    if not dest.exists() or hashlib.sha256(dest.read_bytes()).hexdigest() != item['sha256']:
        temp = dest.with_suffix('.download')
        print('Downloading pinned archive:', item['url'], flush=True)
        with urllib.request.urlopen(item['url'], timeout=120) as response, temp.open('wb') as output:
            shutil.copyfileobj(response, output)
        if hashlib.sha256(temp.read_bytes()).hexdigest() != item['sha256']:
            temp.unlink()
            raise RuntimeError('archive integrity mismatch')
        temp.replace(dest)
    return dest

def prepare():
    system = {'Linux': 'linux', 'Darwin': 'macos'}.get(platform.system())
    arch = {'arm64': 'aarch64', 'aarch64': 'aarch64', 'AMD64': 'x86_64', 'x86_64': 'x86_64'}.get(platform.machine())
    key = f'{system}-{arch}'
    if key not in LOCK['archives']:
        raise RuntimeError(f'No verified JSC archive for {key}; Windows worker remains blocked')
    base = ROOT / '.build' / 'jsc' / key
    base.mkdir(parents=True, exist_ok=True)
    libs = base / 'lib'
    lock_digest = hashlib.sha256(json.dumps(LOCK, sort_keys=True).encode()).hexdigest()
    marker = libs / 'ogma-prepared'
    if marker.exists() and marker.read_text() == lock_digest:
        return libs
    archive = download(LOCK['archives'][key], base / 'engine.tar.gz')
    libs.mkdir(exist_ok=True)
    with tarfile.open(archive) as source:
        source.extractall(libs, filter='data')
    if system == 'linux':
        source_archive = download(LOCK['linuxPicIcu'], base / 'icu-src.tar.gz')
        with tarfile.open(source_archive) as source:
            source.extractall(base, filter='data')
        cwd = base / 'icu' / 'source'
        env = dict(os.environ, CFLAGS='-O2 -fPIC', CXXFLAGS='-O2 -fPIC')
        with (base / 'icu-build.log').open('w') as log:
            subprocess.run(['./configure', '--disable-shared', '--enable-static', '--with-data-packaging=static',
                '--disable-tests', '--disable-samples'], cwd=cwd, env=env, stdout=log, stderr=log, check=True)
            subprocess.run(['make', '-j' + str(min(os.cpu_count() or 2, 8))], cwd=cwd, env=env, stdout=log, stderr=log, check=True)
        for name in ('libicuuc.a', 'libicui18n.a', 'libicudata.a'):
            shutil.copy2(cwd / 'lib' / name, libs / name)
    marker.write_text(lock_digest)
    return libs

if __name__ == '__main__':
    print(prepare())
