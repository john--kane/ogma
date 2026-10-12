"""HTTP/native trust-boundary proof using an observable fake inference engine."""
import hashlib
import json
import os
import signal
import subprocess
import time
import urllib.error
import urllib.request
import unittest
import test_model_cli as fixtures
CLI = fixtures.CLI


class ModelApi(unittest.TestCase):
    setUp = fixtures.ModelProcesses.setUp
    tearDown = fixtures.ModelProcesses.tearDown
    configure = fixtures.ModelProcesses.configure
    args = fixtures.ModelProcesses.args
    start = fixtures.ModelProcesses.start
    wait_for_server = fixtures.ModelProcesses.wait_for_server
    assert_reaped = fixtures.ModelProcesses.assert_reaped

    def host(self, *extra):
        process = self.start('run', str(self.model), extra=extra)
        pid = self.wait_for_server(process)
        ready = json.loads(process.stdout.readline())
        self.assertEqual(ready['status'], 'ok', ready)
        return process, pid

    def http(self, path, value=None, headers=None):
        request = urllib.request.Request(f'http://127.0.0.1:{self.port}{path}',
            data=None if value is None else json.dumps(value).encode(),
            headers={'Content-Type': 'application/json', **(headers or {})})
        return urllib.request.urlopen(request, timeout=5)

    def test_tool_compression_exact_recovery_and_prompt_invariance(self):
        self.host()
        prompt = '  preserve the prompt\nexact $code [brackets]  '
        original = 'error: exact code at middle\n'.join('line %04d\n' % n for n in range(2500))
        payload = {'model': 'ignored', 'messages': [
            {'role': 'user', 'content': prompt}, {'role': 'tool', 'content': original}],
            'ogma_caveman': 'ultra'}
        with self.http('/v1/chat/completions', payload) as r: self.assertEqual(r.status, 200)
        sent = json.loads((self.root / 'requests.jsonl').read_text().splitlines()[0])
        self.assertEqual(sent['messages'][1]['content'], prompt)
        self.assertEqual(sent['messages'][0]['role'], 'system')
        self.assertLess(len(sent['messages'][2]['content']), 8192)
        self.assertNotIn('ogma_caveman', sent)
        with self.http('/ogma/context/' + hashlib.sha256(original.encode()).hexdigest()) as r:
            self.assertEqual(r.read().decode(), original)

    def test_stream_embeddings_and_native_bindings(self):
        self.host()
        with self.http('/v1/chat/completions', {'messages': [{'role': 'user', 'content': 'prompt'}], 'stream': True}) as r:
            self.assertEqual(r.headers['Content-Type'], 'text/event-stream')
            self.assertIn(b'data: [DONE]', r.read())
        with self.http('/v1/embeddings', {'input': 'exact text'}) as r:
            self.assertEqual(json.load(r)['data'][0]['embedding'], [0.25, -0.5, 1.0])
        result = subprocess.run([str(CLI), 'eval',
            'console.log(await Ogma.modelAsk("JS exact prompt")); console.log(JSON.stringify(await Ogma.modelEmbed("text")));',
            '--allow-model', f'--port={self.port}'], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('answer:JS exact prompt', result.stdout)
        self.assertIn('[0.25,-0.5,1]', result.stdout)
        denied = subprocess.run([str(CLI), 'eval', 'await Ogma.modelAsk("denied")', '--json', f'--port={self.port}'], capture_output=True, text=True, timeout=10)
        self.assertNotEqual(denied.returncode, 0)
        self.assertEqual(json.loads(denied.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')

    def test_browser_origin_and_invalid_extension_fail_before_inference(self):
        self.host()
        for payload, headers in [({'messages': []}, {'Origin': 'https://attacker.invalid'}),
                                 ({'messages': [], 'ogma_caveman': 'wrong'}, {})]:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                self.http('/v1/chat/completions', payload, headers)
            caught.exception.close()
        self.assertFalse((self.root / 'requests.jsonl').exists())

    def test_context_file_needs_read_grant_and_stays_separate(self):
        context = self.root / 'context.txt'
        context.write_text('log exact\n' * 4000)
        common = ['--model=' + str(self.model), '--context=' + str(context)]
        denied = subprocess.run(self.args('ask', 'unaltered', extra=common), capture_output=True, text=True, timeout=10)
        self.assertEqual(json.loads(denied.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')
        granted = subprocess.run(self.args('ask', 'unaltered', extra=common + ['--allow-read=' + str(context)]), capture_output=True, text=True, timeout=10)
        self.assertEqual(granted.returncode, 0, granted.stdout + granted.stderr)
        messages = json.loads((self.root / 'requests.jsonl').read_text().splitlines()[0])['messages']
        self.assertEqual(messages[-1], {'role': 'user', 'content': 'unaltered'})
        self.assertIn('ogma context excerpt', messages[0]['content'])

    def test_idle_engine_crash_ends_session_without_restart(self):
        process = self.start('chat', str(self.model))
        pid = self.wait_for_server(process)
        self.assertEqual(json.loads(process.stdout.readline())['status'], 'ok')
        os.kill(pid, signal.SIGKILL)
        process.wait(timeout=5)
        out, err = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, out + err)
        self.assertIn('OGMA_WORKER_CRASH', out)
        self.assert_reaped(pid)
        self.assertFalse((self.root / 'requests.jsonl').exists())

    def test_inference_crash_is_not_replayed(self):
        self.configure(crashInference=True)
        result = subprocess.run(self.args('ask', 'only once', extra=['--model=' + str(self.model)]), capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len((self.root / 'requests.jsonl').read_text().splitlines()), 1)
        self.assert_reaped(int((self.root / 'server.pid').read_text()))

    def test_host_media_and_body_limits_fail_before_backend(self):
        self.host()
        cases = [({'messages': []}, {'Host': 'evil.invalid'}, 403),
                 ({'messages': [{'role': 'user', 'content': [{'type': 'image_url', 'image_url': {'url': 'http://remote.invalid/a'}}]}]}, {}, 422),
                 ({'messages': [{'role': 'user', 'content': 'x' * (8 * 1024 * 1024)}]}, {}, 413)]
        for payload, headers, expected in cases:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                self.http('/v1/chat/completions', payload, headers)
            self.assertEqual(caught.exception.code, expected)
            self.assertTrue(json.load(caught.exception)['error']['code'].startswith('OGMA_'))
            caught.exception.close()
        self.assertFalse((self.root / 'requests.jsonl').exists())

    def test_busy_request_does_not_persist_context(self):
        import threading
        self.configure(delay=0.6)
        self.host()
        failures = []
        def active():
            try:
                with self.http('/v1/chat/completions', {'messages': [{'role': 'user', 'content': 'first'}]}) as r: r.read()
            except Exception as e: failures.append(e)
        thread = threading.Thread(target=active)
        thread.start()
        until = time.monotonic() + 3
        while not (self.root / 'requests.jsonl').exists() and time.monotonic() < until: time.sleep(0.01)
        context = 'unique large log\n' * 4000
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.http('/v1/chat/completions', {'messages': [{'role': 'user', 'content': 'rejected'}], 'ogma_context': context})
        self.assertEqual(caught.exception.code, 429)
        caught.exception.close()
        thread.join(timeout=3)
        self.assertFalse(failures)
        self.assertFalse((self.root / 'store' / 'contexts' / hashlib.sha256(context.encode()).hexdigest()).exists())

    def test_host_style_default_and_inference_trace(self):
        self.host('--caveman')
        result = subprocess.run([str(CLI), 'model', 'ask', 'exact prompt', '--allow-model', f'--port={self.port}', '--trace', '--json'], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        response = json.loads(result.stdout)
        self.assertEqual(response['trace'][0]['name'], 'model-ask')
        sent = json.loads((self.root / 'requests.jsonl').read_text().splitlines()[0])
        self.assertEqual(len(sent['messages']), 2)
        self.assertEqual(sent['messages'][0]['role'], 'system')
        self.assertEqual(sent['messages'][1], {'role': 'user', 'content': 'exact prompt'})
