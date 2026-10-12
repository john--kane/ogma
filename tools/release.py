#!/usr/bin/env python3
"""Reproducible preview archives; optional signing with a caller's Ed25519 key."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def check_key(path, public=False):
    args = ['openssl', 'pkey', '-in', str(path), '-text', '-noout', '-passin', 'pass:']
    if public:
        args.append('-pubin')
    # Capture key diagnostics, never print private key material.
    result = subprocess.run(args, capture_output=True)
    if result.returncode or b'ED25519' not in result.stdout:
        raise ValueError('a valid Ed25519 key is required')


def package(source, output, name, key=None):
    source = source.resolve(strict=True)
    output = output.resolve()
    if not source.is_dir() or output.is_relative_to(source):
        raise ValueError('input must be a directory; output must be outside input')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', name):
        raise ValueError('release name must be a safe basename')
    if key:
        key = key.resolve(strict=True)
        if key.is_relative_to(source):
            raise ValueError('signing key must be outside packaged input')
        check_key(key)
    entries = sorted(source.rglob('*'), key=lambda path: path.relative_to(source).as_posix())
    if any(path.is_symlink() or not (path.is_file() or path.is_dir()) for path in entries):
        raise ValueError('package input must contain regular files and directories, without symlinks')
    epoch = int(os.environ.get('SOURCE_DATE_EPOCH', '0'))
    if not 0 <= epoch <= 0xffffffff:
        raise ValueError('SOURCE_DATE_EPOCH must be in 0..4294967295')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.ogma-release-', dir=output) as folder:
        staging = Path(folder)
        archive = staging / (name + '.tar.gz')
        with archive.open('wb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=epoch) as zipped:
            with tarfile.open(fileobj=zipped, mode='w', format=tarfile.GNU_FORMAT) as tar:
                for path in [source, *entries]:
                    relative = path.relative_to(source).as_posix()
                    info = tarfile.TarInfo(name if relative == '.' else name + '/' + relative)
                    info.uid = info.gid = 0
                    info.uname = info.gname = ''
                    info.mtime = epoch
                    if path.is_dir():
                        info.type = tarfile.DIRTYPE
                        info.mode = 0o755
                        tar.addfile(info)
                    else:
                        info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                        info.size = path.stat().st_size
                        with path.open('rb') as content:
                            tar.addfile(info, content)
        manifest = staging / (name + '.manifest.json')
        manifest.write_text(json.dumps({'formatVersion': 1, 'artifact': archive.name,
            'sha256': sha256(archive), 'size': archive.stat().st_size,
            'signatureAlgorithm': 'Ed25519' if key else 'unsigned'},
            sort_keys=True, separators=(',', ':')) + '\n', encoding='utf-8')
        signature = staging / (manifest.name + '.sig')
        if key:
            subprocess.run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(key), '-passin', 'pass:',
                            '-in', str(manifest), '-out', str(signature)], check=True, capture_output=True)
        for path in (archive, manifest):
            path.replace(output / path.name)
        destination = output / signature.name
        if key:
            signature.replace(destination)
        else:
            destination.unlink(missing_ok=True)
    return output / manifest.name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, default=Path('artifacts'))
    parser.add_argument('--output', type=Path, default=Path('dist'))
    parser.add_argument('--name', required=True)
    parser.add_argument('--sign-key', type=Path)
    args = parser.parse_args()
    try:
        print(package(args.input, args.output, args.name, args.sign_key))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'release failed: {error}\n')


if __name__ == '__main__':
    main()
