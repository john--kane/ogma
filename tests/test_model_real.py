"""Pinned tiny GGUF integration; download explicitly with prepare_llama --test-fixture."""
import hashlib
import json
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
MODEL = ROOT / '.build/llama-test/stories260K.gguf'
CLI = ROOT / 'artifacts/ogma'


@unittest.skipUnless(MODEL.exists(), 'real model fixture not prepared')
class RealModel(unittest.TestCase):
    def test_pinned_cpu_generation_embeddings_and_cli(self):
        pin = json.loads((ROOT / 'contracts/llama.lock.json').read_text())
        self.assertEqual(hashlib.sha256(MODEL.read_bytes()).hexdigest(), pin['testFixture']['sha256'])
        with tempfile.TemporaryDirectory(prefix='ogma-real-model-') as store:
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
            command = [str(CLI), 'model', 'run', str(MODEL), '--allow-model', '--ctx=512',
                       f'--port={port}', '--model-store=' + store, '--json']
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            def request(path, value=None):
                req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', data=None if value is None else json.dumps(value).encode(), headers={'Content-Type': 'application/json'})
                with urllib.request.urlopen(req, timeout=20) as response: return json.load(response)
            try:
                until = time.monotonic() + 45
                while True:
                    self.assertIsNone(process.poll(), 'model host exited before readiness')
                    try:
                        self.assertEqual(request('/health')['service'], 'ogma-model'); break
                    except (OSError, urllib.error.URLError):
                        if time.monotonic() >= until: self.fail('real model startup timed out')
                        time.sleep(0.02)
                self.assertEqual(json.loads(process.stdout.readline())['status'], 'ok')
                result = request('/v1/chat/completions', {'messages': [{'role': 'user', 'content': 'Once upon a time'}], 'max_tokens': 8})
                self.assertGreater(result['usage']['completion_tokens'], 0)
                self.assertIsInstance(result['choices'][0]['message']['content'], str)
                embedded = request('/v1/embeddings', {'input': ['hello', 'world']})
                self.assertEqual(len(embedded['data']), 2)
                for entry in embedded['data']: self.assertEqual(len(entry['embedding']), 64)
                ask = subprocess.run([str(CLI), 'model', 'ask', 'Once upon a time', '--allow-model', f'--port={port}', '--caveman', '--json'], capture_output=True, text=True, timeout=25)
                self.assertEqual(ask.returncode, 0, ask.stdout + ask.stderr)
                self.assertEqual(json.loads(ask.stdout)['status'], 'ok')
            finally:
                if process.poll() is None: process.send_signal(signal.SIGINT)
                try: process.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill(); process.communicate(timeout=5)
