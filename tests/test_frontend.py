import json
import os
from pathlib import Path
import select
import shutil
import signal
import sys
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts' / ('ogma.exe' if os.name == 'nt' else 'ogma')


def frame(value):
    payload = json.dumps(value, ensure_ascii=False).encode()
    return f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload


def unframe(data):
    values = []
    while data:
        header, data = data.split(b'\r\n\r\n', 1)
        size = int(header.split(b':', 1)[1])
        values.append(json.loads(data[:size]))
        data = data[size:]
    return values


class Connection:
    def __init__(self, *args):
        self.process = subprocess.Popen([str(CLI), *map(str, args)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=ROOT, bufsize=0)

    def send(self, value):
        self.process.stdin.write(frame(value))
        self.process.stdin.flush()

    def receive(self, timeout=30):
        deadline = time.monotonic() + timeout
        def read(size):
            data = b''
            while len(data) < size:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([self.process.stdout], [], [], remaining)[0]:
                    raise AssertionError('Timed out reading frontend frame')
                chunk = os.read(self.process.stdout.fileno(), size - len(data))
                if not chunk:
                    raise AssertionError('Frontend exited before replying')
                data += chunk
            return data
        header = b''
        while not header.endswith(b'\r\n\r\n'):
            header += read(1)
        return json.loads(read(int(header.split(b':', 1)[1])))

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            stream.close()


class FrontendTests(unittest.TestCase):
    def run_cli(self, *args, data):
        self.assertTrue(CLI.exists(), 'Build the NativeAOT CLI first')
        return subprocess.run([str(CLI), *args], input=data, capture_output=True, cwd=ROOT, timeout=45)

    def test_repl_persists_ts_async_functions_and_continues_after_guest_error(self):
        result = self.run_cli('repl', '--json', data=(
            'let answer: number = 40;\n'
            'await Ogma.sleep(1); answer += 2; console.log(answer);\n'
            'function plus(n: number) { return answer + n; }\n'
            'console.log("before-error"); throw new Error("expected");\n'
            'console.log(plus(1));\n.exit\n').encode())
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        replies = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(len(replies), 5)
        self.assertEqual(replies[1]['output'][0]['text'], '42')
        self.assertEqual(replies[3]['status'], 'error')
        self.assertEqual(replies[3]['output'][0]['text'], 'before-error')
        self.assertEqual(replies[4]['output'][0]['text'], '43')

    def test_repl_import_and_expression(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as folder:
            dep = Path(folder) / 'dep.ts'
            dep.write_text('export const answer: number = 42;')
            source = f'import {{ answer }} from "{dep}";\nanswer\n.exit\n'
            result = self.run_cli('repl', '--json', f'--allow-read={folder}', data=source.encode())
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            replies = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(replies[0]['status'], 'ok', replies)
            self.assertEqual(replies[1]['output'][0]['text'], '42', replies)

    def test_repl_async_declarations_destructuring_and_redeclaration(self):
        result = self.run_cli('repl', '--json', data=(
            '/* leading Unicode comment: ☃ */ const value: number = await Promise.resolve(40); const { extra } = { extra: 2 };\n'
            'value + extra\n'
            'let value: number = 7;\n'
            'value\n.exit\n').encode())
        replies = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(replies[1]['output'][0]['text'], '42', replies)
        self.assertEqual(replies[3]['output'][0]['text'], '7', replies)

    def test_repl_timeout_ends_session_without_replay(self):
        result = self.run_cli('repl', '--json', '--timeout-ms=100', data=b'console.log("once"); while(true) {}\nconsole.log("must-not-run");\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(b'must-not-run', result.stdout)

    def test_rpc_preserves_string_and_numeric_identity_and_reports_errors(self):
        messages = [
            {'jsonrpc': '2.0', 'id': 'unicode-\u2603', 'method': 'eval', 'params': {'source': 'console.log("\u2603");'}},
            {'jsonrpc': '2.0', 'id': 17, 'method': 'capabilities'},
            {'jsonrpc': '2.0', 'id': 18, 'method': 'missing'},
            {'jsonrpc': '2.0', 'id': 19, 'method': 'eval', 'params': {'entry': 'wrong'}},
        ]
        result = self.run_cli('rpc', data=b''.join(map(frame, messages)))
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        replies = {v['id']: v for v in unframe(result.stdout)}
        self.assertEqual(replies['unicode-\u2603']['result']['output'][0]['text'], '\u2603')
        self.assertIn('eval', replies[17]['result']['methods'])
        self.assertEqual(replies[18]['error']['code'], -32601)
        self.assertEqual(replies[19]['error']['code'], -32602)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux descendant RSS sampling')
    def test_tool_memory_monitor_includes_helper_descendants(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            shutil.copy2(CLI, root / 'ogma')
            worker = root / 'ogma-worker'
            worker.write_text(f'#!{sys.executable}\nimport sys, subprocess, time\n'
                'sys.stdin.buffer.read()\n'
                f'subprocess.Popen([{sys.executable!r}, "-c", "import time; x=bytearray(160*1024*1024); time.sleep(30)"])\n'
                'time.sleep(30)\n')
            worker.chmod(0o700)
            result = subprocess.run([str(root / 'ogma'), 'check', str(root / 'main.ts'), '--json', '--memory-mb=64', '--timeout-ms=5000'],
                                    capture_output=True, cwd=ROOT, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_RESOURCE_LIMIT', result.stdout)

    def test_rpc_guest_permissions_cannot_be_elevated_by_params(self):
        with tempfile.TemporaryDirectory() as folder:
            secret = Path(folder) / 'secret.txt'
            secret.write_text('secret')
            source = f'console.log(await Ogma.readTextFile({json.dumps(str(secret))}));'
            result = self.run_cli('rpc', data=frame({'jsonrpc': '2.0', 'id': 1, 'method': 'eval',
                'params': {'source': source, 'permissions': {'read': [folder]}}}))
            reply = unframe(result.stdout)[0]
            self.assertEqual(reply['result']['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED', reply)

    def test_lsp_malformed_document_notification_leaves_server_usable(self):
        connection = Connection('lsp')
        try:
            connection.send({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize'})
            connection.receive()
            connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didOpen', 'params': {}})
            connection.send({'jsonrpc': '2.0', 'id': 2, 'method': 'shutdown'})
            self.assertIsNone(connection.receive()['result'])
            connection.send({'jsonrpc': '2.0', 'method': 'exit'})
            self.assertEqual(connection.process.wait(timeout=10), 0)
        finally:
            connection.close()

    def test_rpc_parse_error_and_bound_transport(self):
        invalid = b'{'
        result = self.run_cli('rpc', data=b'Content-Length: 1\r\n\r\n' + invalid + frame({'jsonrpc': '2.0', 'id': 1, 'method': 'capabilities'}))
        replies = unframe(result.stdout)
        self.assertEqual(replies[0]['error']['code'], -32700)
        self.assertEqual(replies[1]['id'], 1)
        for header in (b'Content-Length: 8388609\r\n\r\n', b'Content-Length: 1\r\nContent-Length: 1\r\n\r\n', b'Content-Length: 5\r\n\r\n{}'):
            result = self.run_cli('rpc', data=header)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(unframe(result.stdout)[0]['error']['code'], -32700)

    @unittest.skipUnless(os.name == 'posix', 'Unix Ctrl+C cancellation')
    def test_rpc_ctrl_c_ends_idle_transport(self):
        connection = Connection('rpc')
        try:
            connection.send({'jsonrpc': '2.0', 'id': 1, 'method': 'capabilities'})
            connection.receive()
            connection.process.send_signal(signal.SIGINT)
            self.assertNotEqual(connection.process.wait(timeout=5), 0)
        finally:
            connection.close()

    def test_rpc_cancellation_does_not_block_message_reader(self):
        connection = Connection('rpc', '--timeout-ms=10000')
        try:
            connection.send({'jsonrpc': '2.0', 'id': 'cancel-me', 'method': 'eval', 'params': {'source': 'await Ogma.sleep(5000); console.log("must-not-run");'}})
            connection.send({'jsonrpc': '2.0', 'method': '$/cancelRequest', 'params': {'id': 'cancel-me'}})
            reply = connection.receive()
            self.assertEqual(reply['id'], 'cancel-me')
            self.assertEqual(reply['error']['code'], -32800, reply)
            connection.send({'jsonrpc': '2.0', 'id': 'next', 'method': 'eval', 'params': {'source': 'console.log("alive");'}})
            self.assertEqual(connection.receive()['result']['output'][0]['text'], 'alive')
        finally:
            connection.close()

    def test_lsp_initialize_unsaved_diagnostics_change_close_shutdown(self):
        with tempfile.TemporaryDirectory() as folder:
            entry = Path(folder) / 'main.ts'
            entry.write_text('const diskOnly: string = 10;')
            uri = entry.as_uri()
            connection = Connection('lsp', f'--allow-read={folder}', '--memory-mb=512')
            try:
                connection.send({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {}})
                self.assertEqual(connection.receive()['result']['capabilities']['textDocumentSync']['change'], 1)
                connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didOpen', 'params': {'textDocument': {'uri': uri, 'version': 1, 'languageId': 'typescript', 'text': 'const value: number = "wrong";'}}})
                opened = connection.receive()['params']
                self.assertEqual(opened['version'], 1)
                self.assertTrue(any(d['code'] == 'OGMA_TS2322' and "Type 'string'" in d['message'] for d in opened['diagnostics']), opened)
                connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didChange', 'params': {'textDocument': {'uri': uri, 'version': 2}, 'contentChanges': [{'text': 'const value: number = 2;'}]}})
                changed = connection.receive()['params']
                self.assertEqual(changed['version'], 2)
                self.assertEqual(changed['diagnostics'], [])
                connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didChange', 'params': {'textDocument': {'uri': uri, 'version': 3}, 'contentChanges': [{'text': ''}]}})
                empty = connection.receive()['params']
                self.assertEqual(empty['version'], 3)
                self.assertEqual(empty['diagnostics'], [])
                connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didClose', 'params': {'textDocument': {'uri': uri}}})
                self.assertEqual(connection.receive()['params']['diagnostics'], [])
                connection.send({'jsonrpc': '2.0', 'id': 2, 'method': 'shutdown'})
                self.assertIsNone(connection.receive()['result'])
                connection.send({'jsonrpc': '2.0', 'method': 'exit'})
                self.assertEqual(connection.process.wait(timeout=10), 0)
            finally:
                connection.close()

    def test_lsp_cannot_raise_cli_permissions(self):
        with tempfile.TemporaryDirectory() as folder:
            entry = Path(folder) / 'main.ts'
            entry.write_text('import { secret } from \"./secret.ts\"; console.log(secret);')
            (Path(folder) / 'secret.ts').write_text('export const secret: number = 42;')
            connection = Connection('lsp')
            try:
                connection.send({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize'})
                connection.receive()
                connection.send({'jsonrpc': '2.0', 'method': 'textDocument/didOpen', 'params': {'textDocument': {'uri': entry.as_uri(), 'version': 1, 'text': entry.read_text()}}})
                diagnostics = connection.receive()['params']['diagnostics']
                self.assertTrue(any(d['code'] in ('OGMA_PERMISSION_DENIED', 'OGMA_TS2307') for d in diagnostics), diagnostics)
            finally:
                connection.close()


if __name__ == '__main__':
    unittest.main()
