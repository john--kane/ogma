import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts/ogma'


class Client:
    def __init__(self, entry, *flags):
        self.process = subprocess.Popen([str(CLI), 'mcp', 'serve', str(entry), '--no-cache', *flags],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.buffer = b''

    def send(self, method, params=None, identity=None):
        value = {'jsonrpc': '2.0', 'method': method}
        if params is not None: value['params'] = params
        if identity is not None: value['id'] = identity
        self.process.stdin.write(json.dumps(value).encode() + b'\n')
        self.process.stdin.flush()

    def receive(self, timeout=10):
        deadline = time.monotonic() + timeout
        while b'\n' not in self.buffer:
            left = deadline - time.monotonic()
            if left <= 0 or not select.select([self.process.stdout], [], [], left)[0]:
                raise AssertionError('MCP server did not reply')
            chunk = os.read(self.process.stdout.fileno(), 8192)
            if not chunk: raise AssertionError('MCP stdout closed before response')
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b'\n', 1)
        return json.loads(line)

    def initialize(self):
        self.send('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {},
                                'clientInfo': {'name': 'ogma-tests', 'version': '1'}}, 1)
        value = self.receive()
        assert value['result']['protocolVersion'] == '2025-11-25', value
        self.send('notifications/initialized')
        return value

    def close(self):
        if self.process.poll() is None: self.process.kill()
        self.process.wait()
        for stream in [self.process.stdin, self.process.stdout, self.process.stderr]: stream.close()


class McpTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ogma-mcp-')
        self.root = Path(self.temp.name)
        self.clients = []
        self.entry = self.root / 'server.ts'
        self.entry.write_text('''
Ogma.mcpTool({ name: "echo", description: "Echo exact text", inputSchema: {
  type: "object", properties: { text: { type: "string" } }, required: ["text"], additionalProperties: false
}}, async (args) => { await Ogma.sleep(1); console.log("guest log"); return String(args.text); });
Ogma.mcpTool({ name: "read", description: "Read a granted file", inputSchema: {
  type: "object", properties: { path: { type: "string" } }, required: ["path"]
}}, async (args) => await Ogma.readTextFile(String(args.path)));
Ogma.mcpTool({ name: "wait", description: "Wait", inputSchema: { type: "object" } }, async () => {
  await Ogma.sleep(10000); return "done";
});
''')

    def tearDown(self):
        for client in self.clients: client.close()
        self.temp.cleanup()

    def connect(self, *flags):
        client = Client(self.entry, '--allow-read=' + str(self.entry), *flags)
        self.clients.append(client)
        return client

    def test_scaffold_is_executable_and_never_overwrites(self):
        project = self.root / 'new-server'
        result = subprocess.run([str(CLI), 'mcp', 'init', str(project), '--json'],capture_output=True,text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((project / 'server.ts').is_file())
        checked = subprocess.run([str(CLI), 'check', str(project), '--json'], capture_output=True, text=True)
        self.assertEqual(checked.returncode, 0, checked.stdout + checked.stderr)
        original = (project / 'server.ts').read_bytes()
        again = subprocess.run([str(CLI), 'mcp', 'init', str(project), '--json'],capture_output=True,text=True)
        self.assertNotEqual(again.returncode, 0)
        self.assertEqual((project / 'server.ts').read_bytes(), original)
        client = Client(project / 'server.ts', '--allow-read=' + str(project))
        self.clients.append(client)
        client.initialize()
        client.send('tools/list', identity='list')
        self.assertEqual(client.receive()['result']['tools'][0]['name'], 'echo')
        client.send('tools/call', {'name':'echo','arguments':{'text':'hello'}}, 'echo')
        self.assertEqual(client.receive()['result']['content'], [{'type':'text','text':'hello'}])

    def test_lifecycle_discovery_call_and_stdout_is_protocol_only(self):
        client = self.connect()
        client.send('tools/list', identity='before')
        self.assertIn('error', client.receive())
        init = client.initialize()
        self.assertIn('tools', init['result']['capabilities'])
        client.send('tools/list', identity='list')
        tools = client.receive()['result']['tools']
        self.assertEqual([t['name'] for t in tools], ['echo','read','wait'])
        text = 'exact \u0000 [text]\nwith Unicode \u03bb'
        client.send('tools/call', {'name':'echo','arguments':{'text':text}}, 42)
        result = client.receive()
        self.assertEqual(result['id'], 42)
        self.assertEqual(result['result'], {'content':[{'type':'text','text':text}], 'isError':False})
        client.send('ping', identity='alive')
        self.assertEqual(client.receive(), {'jsonrpc':'2.0','id':'alive','result':{}})

    def test_schema_validation_and_permissions_cannot_be_elevated(self):
        client = self.connect()
        client.initialize()
        for args in [{}, {'text':2}, {'text':'ok','extra':True}]:
            client.send('tools/call', {'name':'echo','arguments':args}, 'invalid')
            self.assertEqual(client.receive()['error']['code'], -32602)
        client.send('tools/call', {'name':'missing','arguments':{}}, 'missing')
        self.assertEqual(client.receive()['error']['code'], -32602)
        secret = self.root / 'secret'; secret.write_text('secret')
        client.send('tools/call', {'name':'read','arguments':{'path':str(secret)},
                                 'permissions':{'read':[str(self.root)]}}, 'elevate')
        self.assertEqual(client.receive()['error']['code'], -32602)
        client.send('tools/call', {'name':'read','arguments':{'path':str(secret)}}, 'deny')
        denied = client.receive()['result']
        self.assertTrue(denied['isError'])
        self.assertIn('OGMA_PERMISSION_DENIED', denied['content'][0]['text'])
        allowed = self.connect('--allow-read='+str(secret)); allowed.initialize()
        allowed.send('tools/call', {'name':'read','arguments':{'path':str(secret)}}, 'read')
        self.assertEqual(allowed.receive()['result']['content'][0]['text'], 'secret')

    def test_cancellation_is_read_while_tool_runs_and_connection_survives(self):
        client = self.connect()
        client.initialize()
        client.send('tools/call', {'name':'wait','arguments':{}}, 'waiting')
        client.send('notifications/cancelled', {'requestId':'waiting','reason':'test'})
        cancelled = client.receive(timeout=3)
        self.assertEqual(cancelled['id'], 'waiting')
        self.assertEqual(cancelled['error']['code'], -32800)
        client.send('tools/call', {'name':'echo','arguments':{'text':'after'}}, 'after')
        self.assertEqual(client.receive()['result']['content'][0]['text'], 'after')

    def test_dry_run_does_not_create_files_or_start_server(self):
        target = self.root / 'dry-server'
        result = subprocess.run([str(CLI),'mcp','init',str(target),'--dry-run'],capture_output=True,text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(target.exists())
        serve = subprocess.run([str(CLI),'mcp','serve',str(self.entry),'--dry-run'],capture_output=True,text=True)
        self.assertEqual(serve.returncode, 0, serve.stdout + serve.stderr)
        self.assertEqual(json.loads(serve.stdout)['entry'], str(self.entry))

    def test_malformed_requests_do_not_break_the_connection(self):
        client = self.connect()
        for payload, code in [(b'{\n', -32700), (b'[]\n', -32600),
                              (b'{"jsonrpc":"2.0","id":null,"method":"ping"}\n', -32600)]:
            client.process.stdin.write(payload)
            client.process.stdin.flush()
            self.assertEqual(client.receive()['error']['code'], code)
        client.initialize()
        client.send('unknown', identity='unknown')
        self.assertEqual(client.receive()['error']['code'], -32601)
        client.send('ping', identity='ping')
        self.assertEqual(client.receive()['result'], {})

    def test_active_request_bound_and_duplicate_ids(self):
        client = self.connect()
        client.initialize()
        for identity in range(10, 14):
            client.send('tools/call', {'name':'wait'}, identity)
        client.send('tools/list', identity=14)
        rejected = client.receive()
        self.assertEqual((rejected['id'], rejected['error']['code']), (14, -32600))
        client.send('tools/call', {'name':'wait'}, 10)
        self.assertEqual(client.receive()['error']['code'], -32600)
        for identity in range(10, 14):
            client.send('notifications/cancelled', {'requestId': identity})
        cancelled = [client.receive() for _ in range(4)]
        self.assertEqual({r['id'] for r in cancelled}, {10,11,12,13})
        self.assertTrue(all(r['error']['code'] == -32800 for r in cancelled))

    def test_timeout_never_replays_side_effect_and_server_survives(self):
        marker = self.root / 'marker'; marker.write_text('')
        self.entry.write_text('''
Ogma.mcpTool({name:"fault",description:"",inputSchema:{type:"object"}}, async () => {
  const path = %s;
  await Ogma.writeTextFile(path, await Ogma.readTextFile(path) + "once");
  while (true) {}
});
Ogma.mcpTool({name:"alive",description:"",inputSchema:{type:"object"}}, () => "alive");
''' % json.dumps(str(marker)))
        client = self.connect('--allow-read='+str(marker), '--allow-write='+str(marker), '--timeout-ms=500')
        client.initialize()
        client.send('tools/call', {'name':'fault'}, 'fault')
        failure = client.receive()
        text = json.dumps(failure)
        self.assertIn('OGMA_TIMEOUT', text)
        self.assertEqual(marker.read_text(), 'once')
        client.send('tools/call', {'name':'alive'}, 'alive')
        self.assertEqual(client.receive()['result']['content'][0]['text'], 'alive')

    def test_bad_registrations_and_external_schemas_fail_closed(self):
        definitions = [
            'Ogma.mcpTool({name:"bad name",description:"",inputSchema:{type:"object"}},()=>"x");',
            'for(let i=0;i<2;i++) Ogma.mcpTool({name:"same",description:"",inputSchema:{type:"object"}},()=>"x");',
            'Ogma.mcpTool({name:"remote",description:"",inputSchema:{type:"object",$ref:"https://example.com/schema"}},()=>"x");',
        ]
        for source in definitions:
            with self.subTest(source=source):
                self.entry.write_text(source)
                client = self.connect(); client.initialize()
                client.send('tools/list', identity='list')
                self.assertIn('error', client.receive())
                client.close(); self.clients.remove(client)

    def test_module_loading_still_requires_explicit_read_grant(self):
        client = Client(self.entry)
        self.clients.append(client); client.initialize()
        client.send('tools/list', identity='deny')
        self.assertIn('OGMA_PERMISSION_DENIED', json.dumps(client.receive()))

    def test_eof_cancels_active_workers_promptly(self):
        client = self.connect(); client.initialize()
        client.send('tools/call', {'name':'wait'}, 'wait')
        client.process.stdin.close()
        self.assertEqual(client.process.wait(timeout=3), 0)
        self.assertEqual(client.process.stdout.read(), b'')

    @unittest.skipUnless(os.name == 'posix' and Path('/proc').is_dir(), 'Linux process inspection')
    def test_worker_crash_is_reported_without_killing_frontend(self):
        import signal
        client = self.connect(); client.initialize()
        client.send('tools/call', {'name':'wait'}, 'crash')
        deadline = time.monotonic() + 3
        child = None
        while time.monotonic() < deadline and child is None:
            for task in (Path('/proc') / str(client.process.pid) / 'task').iterdir():
                try: children = (task / 'children').read_text().split()
                except FileNotFoundError: continue
                if children:
                    child = int(children[0]); break
            if child is None: time.sleep(.01)
        self.assertIsNotNone(child, 'No isolated worker started')
        os.kill(child, signal.SIGKILL)
        failure = client.receive()
        self.assertIn('OGMA_WORKER_CRASH', json.dumps(failure))
        client.send('tools/call', {'name':'echo','arguments':{'text':'after crash'}}, 'after')
        self.assertEqual(client.receive()['result']['content'][0]['text'], 'after crash')

    def test_closed_output_cancels_workers_and_ends_frontend(self):
        client = self.connect(); client.initialize()
        client.process.stdout.close()
        client.send('ping', identity='closed')
        self.assertNotEqual(client.process.wait(timeout=3), 0)

    def test_trace_and_guest_logs_use_stderr(self):
        client = self.connect('--trace'); client.initialize()
        client.send('tools/call', {'name':'echo','arguments':{'text':'traced'}}, 'trace')
        self.assertEqual(client.receive()['result']['content'][0]['text'], 'traced')
        client.process.stdin.close()
        self.assertEqual(client.process.wait(timeout=3), 0)
        diagnostics = client.process.stderr.read().decode()
        self.assertIn('guest log', diagnostics)
        self.assertIn('trace contextInitialization ', diagnostics)
        self.assertEqual(client.process.stdout.read(), b'')

    def test_truncated_and_oversized_lines_terminate_cleanly(self):
        for payload in [b'{', b' ' * (8 * 1024 * 1024 + 1) + b'\n']:
            client = self.connect()
            try:
                client.process.stdin.write(payload)
                client.process.stdin.flush()
            except BrokenPipeError:
                pass
            client.process.stdin.close()
            failure = client.receive()
            self.assertEqual(failure['error']['code'], -32700)
            self.assertNotEqual(client.process.wait(timeout=3), 0)
