#!/usr/bin/env python3
"""Measure real local inference through the shipped ogma CLI and HTTP endpoint."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import socket
import statistics
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def request(url, payload=None, timeout=120):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


def available_port(port=0):
    with socket.socket() as sock:
        # Reject an already-running service so samples cannot target another model.
        sock.bind(('127.0.0.1', port))
        return sock.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def benchmark(args):
    cli, model, binary = (path.resolve(strict=True) for path in (args.cli, args.model, args.llama_server))
    samples = []
    for run in range(args.runs):
        port = available_port(args.port)
        endpoint = f'http://127.0.0.1:{port}'
        command = [str(cli), 'model', 'run', str(model), '--allow-model', f'--port={port}',
                   f'--llama-server={binary}', f'--ctx={args.ctx}', f'--gpu-layers={args.gpu_layers}',
                   '--parallel=1', f'--memory-mb={args.memory_mb}']
        # Each sample owns a fresh model process; startup is measured to HTTP readiness.
        with tempfile.TemporaryFile(mode='w+') as log:
            started = time.perf_counter()
            process = subprocess.Popen(command, stdout=log, stderr=log, start_new_session=True)
            try:
                deadline = started + args.timeout
                while True:
                    if process.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(f'ogma exited before readiness ({process.returncode}): {log.read()[-12000:]}')
                    try:
                        health = request(endpoint + '/health', timeout=1)
                        if health.get('status') == 'ok':
                            break
                    except (OSError, ValueError, urllib.error.URLError):
                        pass
                    if time.perf_counter() >= deadline:
                        raise TimeoutError('model did not become ready')
                    time.sleep(0.05)
                startup_ms = (time.perf_counter() - started) * 1000
                if process.poll() is not None:
                    raise RuntimeError('ogma exited during readiness verification')
                responses = []
                for index in range(args.requests):
                    before = time.perf_counter()
                    response = request(endpoint + '/v1/chat/completions', {
                        'model': model.name, 'messages': [{'role': 'user', 'content': args.prompt}],
                        'max_tokens': args.max_tokens, 'temperature': 0, 'seed': 42, 'stream': False,
                        'cache_prompt': False,
                    }, timeout=args.timeout)
                    elapsed = time.perf_counter() - before
                    usage = response.get('usage', {})
                    generated = usage.get('completion_tokens')
                    if not isinstance(usage.get('prompt_tokens'), int) or not isinstance(generated, int):
                        raise RuntimeError('server response missing actual token usage')
                    responses.append({'request': index + 1, 'wallMs': elapsed * 1000,
                                      'usage': usage, 'timings': response.get('timings'),
                                      'completionTokensPerWallSecond': generated / elapsed,
                                      'finishReason': response['choices'][0].get('finish_reason')})
                samples.append({'run': run + 1, 'startupReadyMs': startup_ms, 'health': health,
                                'requests': responses})
                print(f'Run {run + 1}: ready {startup_ms:.1f} ms', flush=True)
            finally:
                stop(process)
    cpu_flags = None
    if Path('/proc/cpuinfo').exists():
        cpu_flags = next((line.split(':', 1)[1].strip().split() for line in Path('/proc/cpuinfo').read_text().splitlines()
                          if line.startswith('flags')), None)
    build_metadata = binary.parent / 'build.json'
    if not build_metadata.exists():
        build_metadata = binary.parent.parent / 'ogma-prepared.json'
    token_timings = [req['timings'] for sample in samples for req in sample['requests'] if req.get('timings')]
    medians = {'startupReadyMs': statistics.median(sample['startupReadyMs'] for sample in samples),
               'requestWallMs': statistics.median(req['wallMs'] for sample in samples for req in sample['requests']),
               'completionTokensPerWallSecond': statistics.median(req['completionTokensPerWallSecond'] for sample in samples for req in sample['requests'])}
    for key in ('prompt_ms', 'predicted_ms', 'prompt_per_second', 'predicted_per_second'):
        values = [timing[key] for timing in token_timings if isinstance(timing.get(key), (int, float))]
        if values:
            medians['engine_' + key] = statistics.median(values)
    return {
        'schema': 1, 'measuredAtUtc': datetime.now(timezone.utc).isoformat(),
        'transport': 'public ogma model run + loopback OpenAI HTTP',
        'platform': platform.platform(), 'cpu': platform.processor(), 'cpuFlags': cpu_flags,
        'model': {'path': str(model), 'bytes': model.stat().st_size, 'sha256': sha256(model)},
        'cli': {'path': str(cli), 'sha256': sha256(cli)},
        'engine': {'path': str(binary), 'sha256': sha256(binary),
                   'build': json.loads(build_metadata.read_text()) if build_metadata.exists() else None},
        'backend': 'CPU' if args.gpu_layers == 0 else 'external GPU configuration (not independently verified)',
        'configuration': {'ctx': args.ctx, 'gpuLayers': args.gpu_layers, 'parallel': 1,
                          'maxTokens': args.max_tokens, 'prompt': args.prompt,
                          'memoryMb': args.memory_mb, 'cachePrompt': False, 'readinessPollMs': 50},
        'medians': medians,
        'samples': samples,
        'limitations': 'Fresh process startup with operating-system file cache unspecified; engine timings are optional. Tiny fixtures verify protocol and timing only, not model quality.',
    }


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model', type=Path)
    parser.add_argument('--cli', type=Path, default=ROOT / 'artifacts/ogma')
    parser.add_argument('--llama-server', type=Path, default=ROOT / 'artifacts/model/llama-server')
    parser.add_argument('--output', type=Path, default=ROOT / '.build/llama-test/benchmark.json')
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--requests', type=int, default=3)
    parser.add_argument('--max-tokens', type=int, default=32)
    parser.add_argument('--ctx', type=int, default=4096)
    parser.add_argument('--gpu-layers', type=int, default=0)
    parser.add_argument('--memory-mb', type=int, default=4096)
    parser.add_argument('--port', type=int, default=0)
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--prompt', default='Once upon a time')
    args = parser.parse_args()
    if min(args.runs, args.requests, args.max_tokens, args.ctx) < 1 or args.timeout <= 0:
        parser.error('runs, requests, max-tokens, ctx and timeout must be positive')
    report = benchmark(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(args.output)
