import json
import os
from pathlib import Path
import struct
import signal
import time
import shutil
import sys
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
PROTOCOL_VERSION = json.loads((ROOT / 'contracts/ogma.json').read_text())['protocolVersion']
CLI = ROOT / 'artifacts' / ('ogma.exe' if os.name == 'nt' else 'ogma')
WORKER = ROOT / 'artifacts' / ('ogma-worker.exe' if os.name == 'nt' else 'ogma-worker')

class Acceptance(unittest.TestCase):
    def run_cli(self, *args, timeout=15):
        self.assertTrue(CLI.exists(), 'NativeAOT ogma CLI has not been built')
        return subprocess.run([str(CLI), *map(str, args)], capture_output=True, text=True,
                              cwd=ROOT, timeout=timeout)

    def test_capabilities_are_versioned_and_honest(self):
        result = self.run_cli('capabilities', '--json')
        self.assertEqual(result.returncode, 0, result.stderr)
        value = json.loads(result.stdout)
        self.assertEqual(value['protocolVersion'], PROTOCOL_VERSION)
        self.assertEqual(value['engine'], 'JavaScriptCore')
        self.assertEqual(value['isolation'], 'process')
        self.assertFalse(value['features']['nodeCompatibility'])

    def test_ts_esm_and_async_without_build(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'dep.ts').write_text('export const answer: number = 42;')
            (root / 'main.ts').write_text('import { answer } from "./dep.ts";\n'
                'await Ogma.sleep(1); console.log(answer);')
            result = self.run_cli('run', root / 'main.ts', '--allow-read=' + folder, '--json')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            value = json.loads(result.stdout)
            self.assertEqual(value['status'], 'ok')
            self.assertEqual(value['output'][0]['text'], '42')

    def test_entry_is_denied_by_default(self):
        result = self.run_cli('run', ROOT / 'tests/fixtures/hello.ts', '--json')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')

    def test_timeout_kills_guest_without_replay(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'main.ts'
            main.write_text('while (true) {}')
            result = self.run_cli('run', main, '--allow-read=' + folder, '--timeout-ms=200', '--json')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_TIMEOUT')

    def eval_json(self, source, *flags):
        result = self.run_cli('eval', source, '--json', *flags)
        return result, json.loads(result.stdout)

    def test_async_operations_settle_in_completion_order(self):
        result, value = self.eval_json('const first = await Promise.race(['
            'Ogma.sleep(30).then(() => "slow"), Ogma.sleep(1).then(() => "fast")]); console.log(first);')
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(value['output'][0]['text'], 'fast')

    def test_guest_fs_env_and_import_permissions(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'data').write_text('allowed')
            source = f'console.log(await Ogma.readTextFile({json.dumps(str(root / "data"))}));'
            result, value = self.eval_json(source)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')
            result, value = self.eval_json(source, '--allow-read=' + folder)
            self.assertEqual(result.returncode, 0, value)
            self.assertEqual(value['output'][0]['text'], 'allowed')
            result, value = self.eval_json('Ogma.env("OGMA_TEST_SECRET")')
            self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')
            result, value = self.eval_json('await import("https://example.com/x.js")')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_UNSUPPORTED')

    def test_native_test_runner_reports_failure(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'test.ts'
            main.write_text('Ogma.test("async", async () => { await Ogma.sleep(1); });')
            result = self.run_cli('test', main, '--allow-read=' + folder, '--json')
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn('ok async', [x['text'] for x in json.loads(result.stdout)['output']])
            main.write_text('Ogma.test("fail", () => { throw new Error("test failed"); });')
            result = self.run_cli('test', main, '--allow-read=' + folder, '--json')
            self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_unresolved_await_is_not_success(self):
        result, value = self.eval_json('await new Promise(() => {})')
        self.assertNotEqual(result.returncode, 0, value)

    def test_output_is_bounded(self):
        result, value = self.eval_json('console.log("x".repeat(1000))', '--max-output-bytes=100')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_RESOURCE_LIMIT')

    def test_large_unicode_guest_error_is_a_bounded_diagnostic(self):
        result, value = self.eval_json('throw new Error("é".repeat(1000000));')
        self.assertNotEqual(result.returncode, 0)
        diagnostic = value['diagnostics'][0]
        self.assertEqual(diagnostic['code'], 'OGMA_EXECUTION')
        self.assertLess(len(diagnostic['message'].encode()), 17000)
        self.assertTrue(diagnostic['message'].endswith('[truncated]'))

    def test_trusted_abi_executes_the_same_contract(self):
        result, value = self.eval_json('const n: number = 42; console.log(n);', '--trusted-in-process')
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(value['output'][0]['text'], '42')

    def test_guest_cannot_modify_runtime_cache(self):
        with tempfile.TemporaryDirectory() as folder:
            cache = Path(folder) / 'cache'
            seed, _ = self.eval_json('console.log("seed")', '--cache-dir=' + str(cache))
            self.assertEqual(seed.returncode, 0)
            target = next(p for p in cache.rglob('*') if p.is_file() and len(p.name) == 64)
            source = f'await Ogma.writeTextFile({json.dumps(str(target))}, "bad");' 
            result, value = self.eval_json(source, '--cache-dir=' + str(cache), '--allow-write=' + folder)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')

    def test_cache_survives_corruption_and_does_not_bypass_permissions(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            main = root / 'main.ts'
            main.write_text('const n: number = 42; console.log(n);')
            cache = root / 'cache'
            flags = ['--cache-dir=' + str(cache), '--trace', '--json']
            first = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            self.assertEqual(first.returncode, 0, first.stdout)
            second = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            self.assertTrue(any(x['name'] == 'transformCacheHits' and x['count'] == 1 for x in json.loads(second.stdout)['trace']))
            denied = self.run_cli('run', main, *flags)
            self.assertEqual(json.loads(denied.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')
            entries = [p for p in cache.rglob('*') if p.is_file() and len(p.name) == 64]
            self.assertGreater(len(entries), 0)
            for path in entries:
                path.write_bytes(b'partial')
            repaired = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            self.assertEqual(repaired.returncode, 0, repaired.stdout)
            self.assertEqual(json.loads(repaired.stdout)['output'][0]['text'], '42')

    def test_typescript_diagnostics_use_source_maps(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'mapped.ts'
            main.write_text('type N = number;\n\nconst n: N = 42;\n'
                'function fail(value: N): never {\n  throw new Error("mapped failure");\n}\nfail(n);')
            result = self.run_cli('run', main, '--allow-read=' + folder, '--json')
            self.assertNotEqual(result.returncode, 0)
            diagnostic = json.loads(result.stdout)['diagnostics'][0]
            self.assertEqual(diagnostic.get('line'), 5, diagnostic)
            self.assertGreater(diagnostic.get('column', 0), 0)

    def test_graph_cache_revalidates_changed_dependencies_and_permissions(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            main, dependency = root / 'main.ts', root / 'dep.ts'
            main.write_text('import { value } from "./dep.ts"; console.log(value);')
            dependency.write_text('export const value: number = 1;')
            flags = ['--cache-dir=' + str(root / 'cache'), '--trace', '--json']
            self.assertEqual(self.run_cli('run', main, '--allow-read=' + folder, *flags).returncode, 0)
            warm = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            self.assertTrue(any(t['name'] == 'graphCacheHits' and t['count'] == 2 for t in json.loads(warm.stdout)['trace']))
            dependency.write_text('export const value: number = 2;')
            changed = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            value = json.loads(changed.stdout)
            self.assertEqual(value['output'][0]['text'], '2', value)
            self.assertTrue(any(t['name'] == 'graphCacheHits' and t['count'] == 1 for t in value['trace']), value)
            denied = self.run_cli('run', main, '--allow-read=' + str(main), *flags)
            self.assertEqual(json.loads(denied.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')
            # Both metadata and content corruption must degrade to fresh loading.
            for entry in (root / 'cache').rglob('*'):
                if entry.is_file(): entry.write_bytes(b'partial')
            recovered = self.run_cli('run', main, '--allow-read=' + folder, *flags)
            self.assertEqual(json.loads(recovered.stdout)['output'][0]['text'], '2')

    def test_caught_dynamic_import_error_can_continue(self):
        result, value = self.eval_json('try { await import("https://example.com/x.js"); } '
            'catch (e) { console.log("caught"); } console.log("done");')
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual([x['text'] for x in value['output']], ['caught', 'done'])

    def test_test_runner_does_not_accept_unresolved_test(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'test.ts'
            main.write_text('Ogma.test("pending", () => new Promise(() => {}));')
            result = self.run_cli('test', main, '--allow-read=' + folder, '--json')
            self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_guest_output_is_preserved_on_error(self):
        result, value = self.eval_json('console.log("before"); throw new Error("failed");')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([x['text'] for x in value['output']], ['before'])

    def test_nul_strings_survive_worker_and_trusted_abi(self):
        for flags in [(), ('--trusted-in-process',)]:
            result, value = self.eval_json('console.log("a\\0b");', *flags)
            self.assertEqual(result.returncode, 0, value)
            self.assertEqual(value['output'][0]['text'], 'a\0b')
        # A literal NUL in module source previously unwound through a C callback.
        payload = json.loads(self.run_cli('eval', 'console.log(1)', '--dry-run').stdout)
        payload['source'] = '//\0\nconsole.log(1);'
        encoded = json.dumps(payload).encode()
        result = subprocess.run([str(WORKER)], input=struct.pack('<I', len(encoded)) + encoded,
                                capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        value = json.loads(result.stdout[4:])
        self.assertEqual(value['status'], 'ok', value)
        with tempfile.TemporaryDirectory() as folder:
            data = Path(folder) / 'nul.txt'
            data.write_text('a\0b')
            for flags in [(), ('--trusted-in-process',)]:
                result, value = self.eval_json(f'console.log(await Ogma.readTextFile({json.dumps(str(data))}));',
                                              '--allow-read=' + folder, *flags)
                self.assertEqual(result.returncode, 0, value)
                self.assertEqual(value['output'][0]['text'], 'a\0b')

    @unittest.skipUnless(os.name == 'posix', 'Unix preview supervision')
    def test_worker_crash_does_not_replay(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            shutil.copy2(CLI, root / 'ogma')
            counter = root / 'calls'
            worker = root / 'ogma-worker'
            worker.write_text(f'#!/bin/sh\nprintf "called\\n" >> "{counter}"\nexit 17\n')
            worker.chmod(0o700)
            result = subprocess.run([str(root / 'ogma'), 'eval', 'console.log(1)', '--json'],
                                    capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_WORKER_CRASH')
            self.assertEqual(counter.read_text().splitlines(), ['called'])
            # The frontend remains usable after a native worker failure.
            self.assertEqual(self.run_cli('eval', 'console.log(42)').returncode, 0)

    @unittest.skipUnless(os.name == 'posix', 'Unix worker fixture')
    def test_invalid_worker_response_fails_closed(self):
        base = {'protocolVersion': PROTOCOL_VERSION, 'id': 'cli', 'status': 'ok',
                'diagnostics': [], 'output': [], 'trace': []}
        mutations = [dict(base, output=None), dict(base, output=[None]),
                     dict(base, diagnostics=[None]), dict(base, trace=[None]),
                     {k: v for k, v in base.items() if k != 'output'}]
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            shutil.copy2(CLI, root / 'ogma')
            worker = root / 'ogma-worker'
            for response in mutations:
                payload = json.dumps(response).encode()
                worker.write_text(f'#!{sys.executable}\nimport sys, struct\n'
                    f'sys.stdin.buffer.read()\npayload = {payload!r}\n'
                    'sys.stdout.buffer.write(struct.pack("<I", len(payload)) + payload)\n')
                worker.chmod(0o700)
                result = subprocess.run([str(root / 'ogma'), 'eval', '0', '--json'],
                                        capture_output=True, text=True, timeout=5)
                self.assertNotEqual(result.returncode, 0, response)
                self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_PROTOCOL', response)

    @unittest.skipUnless(os.name == 'posix', 'Unix SIGINT cancellation')
    def test_cancellation_terminates_worker(self):
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder) / 'ready'
            source = f'await Ogma.writeTextFile({json.dumps(str(marker))}, "ready"); await Ogma.sleep(60000);'
            process = subprocess.Popen([str(CLI), 'eval', source, '--allow-write=' + folder, '--json'],
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 5
                while not marker.exists() and time.monotonic() < deadline and process.poll() is None:
                    time.sleep(0.01)
                self.assertTrue(marker.exists(), 'guest never started')
                process.send_signal(signal.SIGINT)
                stdout, stderr = process.communicate(timeout=5)
                self.assertNotEqual(process.returncode, 0, stderr)
                self.assertEqual(json.loads(stdout)['diagnostics'][0]['code'], 'OGMA_CANCELLED')
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate()

    def test_external_memory_limit(self):
        result, value = self.eval_json('const chunks = []; while (true) {'
            'const chunk = new Uint8Array(4 * 1024 * 1024); chunk.fill(1); chunks.push(chunk); await Ogma.sleep(5); }',
            '--memory-mb=64', '--timeout-ms=5000')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_RESOURCE_LIMIT', value)

    def test_framing_rejects_oversize_and_unknown_version(self):
        self.assertTrue(WORKER.exists(), 'Rust/JSC worker has not been built')
        result = subprocess.run([str(WORKER)], input=struct.pack('<I', 8 * 1024 * 1024 + 1),
                                capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b'')
        payload = json.dumps({'protocolVersion': 99, 'id': 'test', 'command': 'run',
                              'entry': 'x.ts'}).encode()
        result = subprocess.run([str(WORKER)], input=struct.pack('<I', len(payload)) + payload,
                                capture_output=True, timeout=5)
        size = struct.unpack('<I', result.stdout[:4])[0]
        value = json.loads(result.stdout[4:4+size])
        self.assertEqual(value['diagnostics'][0]['code'], 'OGMA_PROTOCOL')

if __name__ == '__main__':
    unittest.main()
