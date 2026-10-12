#!/usr/bin/env python3
"""Prepare exactly locked adapters and package their private Node host."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'tools/runtime'

def prepare(destination):
    node = shutil.which('node')
    npm = shutil.which('npm')
    if not node or not npm:
        raise RuntimeError('Full MVP build requires Node 26.8.2 and npm for private tooling adapters')
    required = json.loads((SOURCE / 'package.json').read_text())['engines']['node']
    version = subprocess.check_output([node, '--version'], text=True).strip().removeprefix('v')
    if version != required:
        raise RuntimeError(f'Private tooling host must be Node {required}; found {version}')
    digest = hashlib.sha256((SOURCE / 'package-lock.json').read_bytes()).hexdigest()
    marker = SOURCE / 'node_modules/.ogma-lock'
    if not marker.exists() or marker.read_text() != digest:
        subprocess.run([npm, 'ci', '--prefix', str(SOURCE), '--ignore-scripts', '--no-audit', '--no-fund',
                        '--cache', str(ROOT / '.build/npm-cache')], check=True)
        marker.write_text(digest)
    destination.mkdir(parents=True, exist_ok=True)
    for name in ('package.json', 'package-lock.json', 'adapter.mjs'):
        shutil.copy2(SOURCE / name, destination / name)
    # Replacing the package tree avoids retaining files from previous tool pins.
    if (destination / 'node_modules').exists():
        shutil.rmtree(destination / 'node_modules')
    shutil.copytree(SOURCE / 'node_modules', destination / 'node_modules')
    shutil.copytree(ROOT / 'types', destination / 'types', dirs_exist_ok=True)
    shutil.copy2(node, destination / 'node')
    license = Path(node).resolve().parent.parent / 'LICENSE'
    if license.exists():
        shutil.copy2(license, destination / 'NODE-LICENSE')
    else:
        raise RuntimeError('Node distribution LICENSE must accompany its private tooling host')
    print(f'Packaged private tooling host Node {version} and integrity-locked adapters')
