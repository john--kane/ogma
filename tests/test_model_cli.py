import json
import os
from pathlib import Path
import subprocess
import signal
import socket
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts' / ('ogma.exe' if os.name == 'nt' else 'ogma')


class ModelCli(unittest.TestCase):
    def cli(self, *args, input=None):
        return subprocess.run([str(CLI), *args], input=input, capture_output=True,
                              text=True, timeout=20)

    def request(self, *args):
        result = self.cli(*args, '--dry-run')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return json.loads(result.stdout)

    def test_model_permission_required_even_for_dry_run(self):
        for args in [('model', 'run', 'fixture.gguf'), ('model', 'chat'), ('model', 'ask', 'hello')]:
            with self.subTest(args=args):
                result = self.cli(*args, '--dry-run', '--json')
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')

    def test_model_command_defaults_and_exact_prompt(self):
        prompt = 'Keep  spaces\nand [literal] code: $x'
        value = self.request('model', 'ask', prompt, '--allow-model')
        self.assertEqual(value['command'], 'model-ask')
        self.assertEqual(value['source'], prompt)
        self.assertTrue(value['permissions']['model'])
        self.assertEqual(value['limits']['timeoutMs'], 120000)
        self.assertEqual(value['limits']['maxMemoryMb'], 4096)
        self.assertEqual(value['model'], dict(model='', server='', store='', context='',
                                             ctx=4096, gpuLayers=0, parallel=1, port=8080, caveman=''))

    def test_prompt_starting_with_dash_uses_end_of_options(self):
        result = self.cli('model', 'ask', '--allow-model', '--dry-run', '--', '--literal prompt')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        value = json.loads(result.stdout)
        self.assertEqual(value['source'], '--literal prompt')

    def test_model_flags_and_js_memory_defaults(self):
        value = self.request('model', 'ask', 'hi', '--allow-model',
                             '--model=hf:owner/model::file.gguf', '--ctx=8192', '--gpu-layers=-1',
                             '--parallel=2', '--port=18080', '--caveman-ultra', '--memory-mb=65536')
        self.assertEqual(value['model']['model'], 'hf:owner/model::file.gguf')
        self.assertEqual(value['model']['gpuLayers'], -1)
        self.assertEqual(value['model']['caveman'], 'ultra')
        self.assertEqual(value['limits']['maxMemoryMb'], 65536)
        default = self.request('eval', '1', '--allow-model')
        self.assertEqual(default['limits']['maxMemoryMb'], 256)
        self.assertEqual(default['limits']['timeoutMs'], 30000)
        rejected = self.cli('eval', '1', '--allow-model', '--model=wrong.gguf', '--dry-run', '--json')
        self.assertEqual(rejected.returncode, 1)
        self.assertEqual(json.loads(rejected.stdout)['diagnostics'][0]['code'], 'OGMA_INVALID_ARGUMENT')

    def test_incompatible_flags_and_bounds(self):
        for extra in [('--trusted-in-process',), ('--sandbox=linux',), ('--port=0',),
                      ('--parallel=0',), ('--gpu-layers=-2',), ('--memory-mb=65537',),
                      ('--caveman', '--caveman-ultra')]:
            with self.subTest(extra=extra):
                result = self.cli('model', 'chat', '--allow-model', '--dry-run', '--json', *extra)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_INVALID_ARGUMENT')
        result = self.cli('eval', '1', '--memory-mb=8192', '--dry-run', '--json')
        self.assertEqual(result.returncode, 1)

    def test_selection_and_path_options(self):
        value = self.request('model', 'chat', 'fixture.gguf', '--allow-model',
                             '--llama-server=./fake-server', '--model-store=./models', '--context=./context.txt')
        self.assertEqual(value['model']['model'], 'fixture.gguf')
        for field, path in [('server', 'fake-server'), ('store', 'models'), ('context', 'context.txt')]:
            self.assertEqual(value['model'][field], str((Path.cwd() / path).resolve()))
        result = self.cli('model', 'run', 'first.gguf', '--model=second.gguf', '--allow-model', '--dry-run', '--json')
        self.assertEqual(result.returncode, 1)


# An explicit executable override exercises the same worker/process boundary as
# llama.cpp without requiring a downloaded model or GPU in CLI acceptance tests.
FAKE_SERVER = r"""#!/usr/bin/python3
import json, os, sys, threading, time
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
base = Path(__file__).parent
config = json.loads((base / 'config.json').read_text())
(base / 'server.pid').write_text(str(os.getpid()))
(base / 'server.args').write_text(json.dumps(sys.argv[1:]))
args = sys.argv[1:]
port = int(args[args.index('--port') + 1])
held = []
if config.get('idleMemory'):
    def grow():
        time.sleep(1)
        held.append(bytearray(config['idleMemory'] * 1024 * 1024))
    threading.Thread(target=grow, daemon=True).start()
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def send_json(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        self.send_json({'status': 'ok', 'data': [{'id': 'fixture', 'object': 'model'}]})
    def do_POST(self):
        value = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with (base / 'requests.jsonl').open('a') as log:
            log.write(json.dumps(value) + '\n')
        if config.get('crashInference'): os._exit(89)
        time.sleep(config.get('delay', 0))
        if self.path == '/v1/embeddings':
            self.send_json({'data': [{'index': 0, 'embedding': [0.25, -0.5, 1.0]}]}); return
        if value.get('stream'):
            body = b'data: {"choices":[{"delta":{"content":"hello"}}]}\\n\\ndata: [DONE]\\n\\n'
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body); return
        prompt = next(m['content'] for m in reversed(value.get('messages', [])) if m['role'] == 'user')
        self.send_json({'id': 'fake', 'object': 'chat.completion',
                        'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'answer:' + prompt}, 'finish_reason': 'stop'}],
                        'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}})
ThreadingHTTPServer(('127.0.0.1', port), Handler).serve_forever()
"""


@unittest.skipIf(os.name == 'nt', 'executable fixture uses a Unix shebang')
class ModelProcesses(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ogma-model-cli-')
        self.root = Path(self.temp.name)
        self.server = self.root / 'fake-server'
        self.server.write_text(FAKE_SERVER)
        self.server.chmod(0o700)
        self.model = self.root / 'fixture.gguf'
        self.model.write_bytes(b'GGUF' + b'\0' * 28)
        self.configure()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            self.port = sock.getsockname()[1]
        self.children = []

    def configure(self, **settings):
        (self.root / 'config.json').write_text(json.dumps(settings))

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=5)
        pidfile = self.root / 'server.pid'
        if pidfile.exists():
            try: os.kill(int(pidfile.read_text()), signal.SIGKILL)
            except ProcessLookupError: pass
        self.temp.cleanup()

    def args(self, command, *positionals, extra=()):
        return [str(CLI), 'model', command, *positionals, '--allow-model',
                '--llama-server=' + str(self.server), '--model-store=' + str(self.root / 'store'),
                '--port=' + str(self.port), '--json', *extra]

    def start(self, command, *positionals, extra=()):
        process = subprocess.Popen(self.args(command, *positionals, extra=extra),
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        self.children.append(process)
        return process

    def wait_for_server(self, process):
        until = time.monotonic() + 10
        while time.monotonic() < until:
            if (self.root / 'server.pid').exists():
                return int((self.root / 'server.pid').read_text())
            if process.poll() is not None:
                out, err = process.communicate()
                self.fail('model process exited before fake server started: ' + out + err)
            time.sleep(0.02)
        self.fail('fake server did not start')

    def assert_reaped(self, pid):
        until = time.monotonic() + 3
        while time.monotonic() < until:
            try: os.kill(pid, 0)
            except ProcessLookupError: return
            # Linux orphan zombies cannot execute or retain memory.
            stat = Path('/proc') / str(pid) / 'stat'
            if stat.exists() and stat.read_text().split()[2] == 'Z': return
            time.sleep(0.02)
        self.fail('llama-server still running after model session ended')

    def test_ask_preserves_prompt_and_caveman_is_system_message(self):
        prompt = 'Keep  spaces\nand [literal] code: $x'
        result = subprocess.run(self.args('ask', prompt, extra=(
            '--model=' + str(self.model), '--caveman')), capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        value = json.loads(result.stdout)
        self.assertIn('answer:' + prompt, [item['text'] for item in value['output']])
        messages = json.loads((self.root / 'requests.jsonl').read_text().splitlines()[0])['messages']
        self.assertEqual([m['content'] for m in messages if m['role'] == 'user'], [prompt])
        self.assertTrue(any(m['role'] == 'system' for m in messages))
        self.assert_reaped(int((self.root / 'server.pid').read_text()))

    def test_chat_keeps_history_and_exit_reaps_server(self):
        process = self.start('chat', str(self.model))
        pid = self.wait_for_server(process)
        out, err = process.communicate('first\nsecond\n.exit\n', timeout=15)
        self.assertEqual(process.returncode, 0, out + err)
        requests = [json.loads(line) for line in (self.root / 'requests.jsonl').read_text().splitlines()]
        self.assertEqual(len(requests), 2)
        self.assertEqual([m['content'] for m in requests[1]['messages'] if m['role'] == 'user'], ['first', 'second'])
        self.assertTrue(any(m['role'] == 'assistant' and m['content'] == 'answer:first' for m in requests[1]['messages']))
        self.assert_reaped(pid)

    def test_run_survives_closed_stdin_until_ctrl_c(self):
        process = self.start('run', str(self.model))
        pid = self.wait_for_server(process)
        process.stdin.close(); process.stdin = None
        time.sleep(1.3)
        self.assertIsNone(process.poll(), 'model run must own the server after stdin EOF')
        process.send_signal(signal.SIGINT)
        out, err = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, out + err)
        self.assert_reaped(pid)

    def test_idle_chat_ctrl_c_reaps_server(self):
        process = self.start('chat', str(self.model))
        pid = self.wait_for_server(process)
        time.sleep(0.3)
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
        out, err = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 0, out + err)
        self.assert_reaped(pid)

    @unittest.skipUnless(os.path.isdir('/proc/self/task'), 'Linux process inspection')
    def test_idle_chat_descendant_memory_is_limited(self):
        self.configure(idleMemory=384)
        process = self.start('chat', str(self.model), extra=('--memory-mb=256',))
        pid = self.wait_for_server(process)
        # Keep stdin open without prompts: this must exercise idle supervision.
        process.wait(timeout=10)
        out, err = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 1, out + err)
        diagnostics = [d for line in out.splitlines() for d in json.loads(line)['diagnostics']]
        self.assertTrue(any(d['code'] == 'OGMA_RESOURCE_LIMIT' for d in diagnostics), out + err)
        self.assert_reaped(pid)

    @unittest.skipUnless(os.path.isdir('/proc/self/task'), 'Linux process inspection')
    def test_idle_worker_crash_ends_chat_without_replay(self):
        process = self.start('chat', str(self.model))
        pid = self.wait_for_server(process)
        workers = []
        for task in (Path('/proc') / str(process.pid) / 'task').iterdir():
            workers.extend(int(child) for child in (task / 'children').read_text().split())
        self.assertEqual(len(set(workers)), 1)
        os.kill(workers[0], signal.SIGKILL)
        process.wait(timeout=10)
        out, err = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 1, out + err)
        diagnostics = [d for line in out.splitlines() for d in json.loads(line)['diagnostics']]
        self.assertTrue(any(d['code'] == 'OGMA_WORKER_CRASH' for d in diagnostics), out + err)
        self.assertFalse((self.root / 'requests.jsonl').exists(), 'idle worker crash must not infer or replay')
        self.assert_reaped(pid)

    def test_timeout_does_not_replay_inference(self):
        self.configure(delay=3)
        result = subprocess.run(self.args('ask', 'once', extra=(
            '--model=' + str(self.model), '--timeout-ms=1500')), capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertTrue(any(d['code'] == 'OGMA_TIMEOUT' for d in json.loads(result.stdout)['diagnostics']))
        self.assertEqual(len((self.root / 'requests.jsonl').read_text().splitlines()), 1)
        self.assert_reaped(int((self.root / 'server.pid').read_text()))


if __name__ == '__main__':
    unittest.main()
