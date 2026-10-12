#!/usr/bin/env python3
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from prepare_jsc import prepare
from prepare_tools import prepare as prepare_tools
from prepare_llama import package as package_llama

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--debug-worker', action='store_true')
parser.add_argument('--core-only', action='store_true', help='Skip llama.cpp preparation and model packaging')
parser.add_argument('--llama-jobs', type=int, default=None)
args = parser.parse_args()
subprocess.run(['python3', 'tools/generate_contracts.py', '--check'], cwd=ROOT, check=True)
libs = prepare()
env = dict(os.environ, RUST_JSC_CUSTOM_BUILD_PATH=str(libs))
command = ['cargo', 'build', '--locked', '-p', 'ogma-runtime']
if not args.debug_worker: command.append('--release')
subprocess.run(command, cwd=ROOT, env=env, check=True)
subprocess.run(['dotnet', 'publish', 'src/Ogma.Cli', '-c', 'Release', '-o', 'artifacts'], cwd=ROOT, check=True)
folder = ROOT / 'target' / ('debug' if args.debug_worker else 'release')
for name in ('ogma-worker', 'libogma_runtime.dylib' if os.uname().sysname == 'Darwin' else 'libogma_runtime.so'):
    shutil.copy2(folder / name, ROOT / 'artifacts' / name)
shutil.copytree(ROOT / 'types', ROOT / 'artifacts' / 'types', dirs_exist_ok=True)
shutil.copytree(ROOT / 'contracts', ROOT / 'artifacts' / 'contracts', dirs_exist_ok=True)
shutil.copy2(ROOT / 'README.md', ROOT / 'artifacts' / 'README.md')
docs = ROOT / 'artifacts' / 'docs'
docs.mkdir(parents=True, exist_ok=True)
for name in ('sandbox-and-releases.md', 'mvp-verification.md', 'mcp.md') + (() if args.core_only else ('model-hosting.md',)):
    shutil.copy2(ROOT / 'docs' / name, docs / name)
shutil.copytree(ROOT / 'docs' / 'benchmarks', docs / 'benchmarks', dirs_exist_ok=True)
prepare_tools(ROOT / 'artifacts/tooling')
if not args.core_only:
    package_llama(ROOT / 'artifacts/model', args.llama_jobs)
print('Built artifacts/ogma with pinned JavaScriptCore. Run python3 tools/test_core.py')
