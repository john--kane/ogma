#!/usr/bin/env python3
"""Verify a release against a caller-supplied public key before unpacking it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
from release import check_key


def bounded_file(path, limit):
    flags = os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0)
    descriptor = os.open(path, flags)
    with os.fdopen(descriptor, 'rb') as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError('verification inputs must be regular files')
        content = source.read(limit + 1)
        if len(content) > limit:
            raise ValueError('verification input exceeds size limit')
        return content


def artifact_digest(path, expected_size, snapshot=None):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0))
    with os.fdopen(descriptor, 'rb') as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != expected_size:
            raise ValueError('artifact type or length mismatch')
        from contextlib import nullcontext
        with snapshot.open('xb') if snapshot else nullcontext(None) as output:
            digest = hashlib.sha256()
            remaining = expected_size
            while remaining:
                block = source.read(min(1024 * 1024, remaining))
                if not block:
                    raise ValueError('artifact truncated during verification')
                digest.update(block)
                if output: output.write(block)
                remaining -= len(block)
            if source.read(1):
                raise ValueError('artifact grew during verification')
            return digest.hexdigest()


def verify(manifest, public_key=None, allow_unsigned=False, *, snapshot_directory=None):
    manifest_bytes = bounded_file(manifest, 65536)
    metadata = json.loads(manifest_bytes)
    if not isinstance(metadata, dict) or set(metadata) != {'formatVersion', 'artifact', 'sha256', 'size', 'signatureAlgorithm'}:
        raise ValueError('invalid manifest fields')
    name = metadata['artifact']
    if (type(metadata['formatVersion']) is not int or metadata['formatVersion'] != 1 or not isinstance(name, str)
        or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}\.tar\.gz', name)
        or not isinstance(metadata['sha256'], str) or not re.fullmatch('[0-9a-f]{64}', metadata['sha256'])
        or type(metadata['size']) is not int or metadata['size'] <= 0):
        raise ValueError('invalid manifest values')
    if metadata['signatureAlgorithm'] == 'Ed25519':
        if public_key is None:
            raise ValueError('signed release requires an independently trusted --public-key')
        check_key(public_key, public=True)
        signature_bytes = bounded_file(Path(str(manifest) + '.sig'), 64)
        if len(signature_bytes) != 64:
            raise ValueError('invalid Ed25519 signature length')
        # OpenSSL must authenticate the exact bytes parsed above. Reopening the
        # caller's path would allow a swap between parsing and verification.
        with tempfile.TemporaryDirectory(prefix='ogma-verify-') as folder:
            snapshot, signature = Path(folder) / 'manifest', Path(folder) / 'signature'
            snapshot.write_bytes(manifest_bytes)
            signature.write_bytes(signature_bytes)
            result = subprocess.run(['openssl', 'pkeyutl', '-verify', '-rawin', '-pubin',
                '-inkey', str(public_key), '-sigfile', str(signature), '-in', str(snapshot)], capture_output=True)
        if result.returncode:
            raise ValueError('release signature verification failed')
    elif metadata['signatureAlgorithm'] != 'unsigned' or not allow_unsigned or public_key is not None:
        raise ValueError('unsigned release requires explicit --allow-unsigned and no public key')
    artifact = manifest.parent / name
    snapshot = snapshot_directory / name if snapshot_directory is not None else None
    if snapshot_directory is not None and metadata['size'] > 2 * 1024**3:
        raise ValueError('preview archive exceeds 2 GiB snapshot limit')
    if artifact_digest(artifact, metadata['size'], snapshot) != metadata['sha256']:
        raise ValueError('artifact SHA-256 mismatch')
    return snapshot if snapshot is not None else artifact


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=Path)
    parser.add_argument('--public-key', type=Path)
    parser.add_argument('--allow-unsigned', action='store_true')
    args = parser.parse_args()
    try:
        print(f'verified: {verify(args.manifest, args.public_key, args.allow_unsigned)}')
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'verification failed: {error}\n')


if __name__ == '__main__':
    main()
