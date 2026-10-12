#!/usr/bin/env python3
"""Measure process completion with empty/populated transform caches, not OS cold start."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]


def distribution(values):
    ordered = sorted(values)
    percentile = lambda p: ordered[max(0, math.ceil(p * len(ordered)) - 1)]
    return {'samples': len(values), 'minMs': min(values), 'medianMs': statistics.median(values),
            'p95Ms': percentile(.95), 'p99Ms': percentile(.99), 'maxMs': max(values),
            'meanMs': statistics.mean(values), 'rawMs': values}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--samples', type=int, default=30)
    parser.add_argument('--output', type=Path, default=ROOT / 'artifacts/startup.json')
    args = parser.parse_args()
    if not 5 <= args.samples <= 10000:
        parser.error('--samples must be in 5..10000')
    cli = ROOT / 'artifacts/ogma'
    worker = ROOT / 'artifacts/ogma-worker'
    version = json.loads(subprocess.check_output([str(cli), 'version', '--json'], text=True))
    results = {name: [] for name in ('cliOnly', 'cliParsing', 'emptyTransformCache', 'populatedTransformCache')}
    phases = {name: {} for name in ('emptyTransformCache', 'populatedTransformCache')}
    sources = {'main.ts': 'import { answer } from "./dep.ts"; console.log(answer);\n',
               'dep.ts': 'export const answer: number = 42;\n'}
    with tempfile.TemporaryDirectory(prefix='ogma-bench-') as folder:
        root = Path(folder)
        for name, source in sources.items():
            (root / name).write_text(source)
        command = [str(cli), 'run', str(root / 'main.ts'), '--allow-read=' + folder, '--json', '--trace']

        def run(cache, expected_hits, phase_mode=None):
            started = time.perf_counter_ns()
            response = subprocess.run(command + ['--cache-dir=' + str(cache)], capture_output=True,
                                      text=True, check=True, timeout=15)
            elapsed = (time.perf_counter_ns() - started) / 1_000_000
            value = json.loads(response.stdout)
            if value['status'] != 'ok' or value['output'][0]['text'] != '42':
                raise RuntimeError('benchmark workload did not execute correctly')
            if not any(event['name'] == 'transformCacheHits' and event['count'] == expected_hits for event in value['trace']):
                raise RuntimeError('benchmark cache state did not match requested mode')
            expected_graph_hits = expected_hits
            if not any(event['name'] == 'graphCacheHits' and event['count'] == expected_graph_hits for event in value['trace']):
                raise RuntimeError('benchmark graph cache state did not match requested mode')
            if phase_mode:
                for event in value['trace']:
                    if event['name'] in ('contextInitialization', 'moduleLoading', 'typescriptTransform', 'execution'):
                        phases[phase_mode].setdefault(event['name'], []).append(event['elapsedMicros'] / 1000)
            return elapsed

        warm = root / 'warm'
        run(warm, 0)
        for index in range(args.samples):
            started = time.perf_counter_ns()
            subprocess.run([str(cli), 'capabilities', '--json'], capture_output=True, check=True, timeout=15)
            results['cliOnly'].append((time.perf_counter_ns() - started) / 1_000_000)
            started = time.perf_counter_ns()
            parsed = subprocess.run([str(cli), 'run', str(root / 'main.ts'), '--allow-read', folder,
                                     '--timeout-ms', '30000', '--no-cache', '--dry-run'],
                                    capture_output=True, text=True, check=True, timeout=15)
            results['cliParsing'].append((time.perf_counter_ns() - started) / 1_000_000)
            parsed_request = json.loads(parsed.stdout)
            if parsed_request['command'] != 'run' or parsed_request['permissions']['read'] != [folder] or parsed_request['cacheDirectory'] != '':
                raise RuntimeError('CLI parsing benchmark produced an invalid request')
            # Alternate order to reduce systematic drift between the two modes.
            modes = [('emptyTransformCache', root / f'cold-{index}', 0),
                     ('populatedTransformCache', warm, 2)]
            for name, cache, hits in modes[::1 if index % 2 == 0 else -1]:
                results[name].append(run(cache, hits, name))
    cpu = platform.processor()
    cpuinfo = Path('/proc/cpuinfo')
    if cpuinfo.exists():
        cpu = next((line.split(':', 1)[1].strip() for line in cpuinfo.read_text().splitlines()
                    if line.startswith('model name')), cpu)
    report = {'schemaVersion': 1, 'kind': 'ogma-startup', 'measuredAt': datetime.now(timezone.utc).isoformat(),
              'version': version, 'environment': {'os': platform.platform(), 'machine': platform.machine(),
                  'cpu': cpu, 'logicalCpus': os.cpu_count(), 'python': platform.python_version()},
              'method': 'Wall time from process launch to exit; NativeAOT CLI plus fresh isolated worker. '
                  'Caches are empty/populated transform caches. OS page caches are uncontrolled. '
                  'No snapshot, engine bytecode cache, daemon or performance threshold. '
                  'cliParsing measures launch, parsing and dry-run JSON serialization without starting a worker. '
                  'Phase timings use --trace and include its instrumentation cost. No model process is started.',
              'workload': sources, 'binaries': {path.name: {'bytes': path.stat().st_size,
                  'sha256': hashlib.sha256(path.read_bytes()).hexdigest()} for path in (cli, worker)},
              'results': {name: distribution(values) for name, values in results.items()},
              'phaseTimings': {mode: {name: distribution(values) for name, values in timings.items()}
                               for mode, timings in phases.items()}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    for name, value in report['results'].items():
        print(f'{name}: median={value["medianMs"]:.2f}ms p95={value["p95Ms"]:.2f}ms n={value["samples"]}')
    print(args.output)


if __name__ == '__main__':
    main()
