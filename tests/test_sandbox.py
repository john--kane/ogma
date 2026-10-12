import json
import os
import tempfile
import platform
from pathlib import Path
import shutil
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(platform.system() == 'Linux', 'Linux sandbox prototype')
class SandboxTests(unittest.TestCase):
    def test_requested_sandbox_never_falls_back(self):
        self.assertTrue((ROOT / 'artifacts/ogma').exists(), 'build CLI first')
        result = subprocess.run([str(ROOT / 'artifacts/ogma'), 'eval', 'console.log("sandboxed")',
            '--sandbox=linux', '--no-cache', '--json'], capture_output=True, text=True, timeout=15)
        response = json.loads(result.stdout)
        bwrap = shutil.which('bwrap')
        if bwrap:
            probe = subprocess.run([bwrap, '--unshare-all', '--as-pid-1', '--die-with-parent',
                '--ro-bind', '/', '/', '/usr/bin/true'], capture_output=True, timeout=10)
        if not bwrap or probe.returncode != 0:
            self.assertNotEqual(os.environ.get('OGMA_REQUIRE_SANDBOX'), '1', 'Combined sandbox is required on this CI host but unavailable')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(response['diagnostics'][0]['code'], 'OGMA_SANDBOX_UNAVAILABLE')
            self.assertEqual(response['output'], [])
        else:
            self.assertEqual(result.returncode, 0, response)
            self.assertEqual(response['output'][0]['text'], 'sandboxed')

    def test_sandbox_cannot_be_combined_with_trusted_execution(self):
        result = subprocess.run([str(ROOT / 'artifacts/ogma'), 'eval', 'console.log("bad")',
            '--sandbox=linux', '--trusted-in-process', '--json'], capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_INVALID_ARGUMENT')

    def require_namespaces(self):
        probe = subprocess.run(['/usr/bin/bwrap', '--unshare-all', '--as-pid-1', '--die-with-parent',
            '--ro-bind', '/', '/', '--', '/usr/bin/true'], capture_output=True, timeout=10) if Path('/usr/bin/bwrap').exists() else None
        if probe is None or probe.returncode != 0:
            if os.environ.get('OGMA_REQUIRE_SANDBOX') == '1': self.fail('required combined sandbox unavailable')
            self.skipTest('namespace creation unavailable; fail-closed behavior covered separately')

    def sandbox(self, source, *flags):
        return subprocess.run([str(ROOT / 'artifacts/ogma'), 'eval', source, '--sandbox=linux',
            '--no-cache', '--json', *flags], capture_output=True, text=True, timeout=15)

    def test_combined_jsc_tokio_files_and_verified_cache(self):
        self.require_namespaces()
        with tempfile.TemporaryDirectory(prefix='ogma-sandbox-') as folder:
            root = Path(folder)
            output = root / 'output'; output.mkdir()
            data = root / 'data.txt'; data.write_text('granted text')
            (root / 'dep.ts').write_text('export const answer: number = 42;')
            main = root / 'main.ts'
            main.write_text('import { answer } from "./dep.ts"; await Ogma.sleep(2); '
                f'await Ogma.writeTextFile({json.dumps(str(output / "answer.txt"))}, String(answer)); '
                f'console.log(await Ogma.readTextFile({json.dumps(str(data))}));')
            command = [str(ROOT / 'artifacts/ogma'), 'run', str(main), '--sandbox=linux',
                '--allow-read=' + folder, '--allow-write=' + str(output), '--cache-dir=' + str(root / 'cache'), '--json', '--trace']
            for expected in [0, 2]:
                completed = subprocess.run(command, capture_output=True, text=True, timeout=15)
                self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
                response = json.loads(completed.stdout)
                self.assertEqual(response['output'][0]['text'], 'granted text')
                for phase in ['transformCacheHits', 'graphCacheHits']:
                    self.assertTrue(any(t['name'] == phase and t['count'] == expected for t in response['trace']))
            self.assertEqual((output / 'answer.txt').read_text(), '42')
            link = root / 'escape'; link.symlink_to('/etc/passwd')
            denied = self.sandbox(f'await Ogma.readTextFile({json.dumps(str(link))});', '--allow-read=' + folder)
            self.assertNotEqual(denied.returncode, 0)
            self.assertEqual(json.loads(denied.stdout)['diagnostics'][0]['code'], 'OGMA_PERMISSION_DENIED')

    def test_combined_timeout_does_not_replay_side_effect(self):
        self.require_namespaces()
        with tempfile.TemporaryDirectory(prefix='ogma-sandbox-timeout-') as folder:
            marker = Path(folder) / 'calls.txt'
            marker.write_text('')
            source = f'await Ogma.writeTextFile({json.dumps(str(marker))}, await Ogma.readTextFile({json.dumps(str(marker))}) + "once"); while (true) {{}}'
            completed = self.sandbox(source, '--allow-read=' + folder, '--allow-write=' + folder, '--timeout-ms=500')
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(json.loads(completed.stdout)['diagnostics'][0]['code'], 'OGMA_TIMEOUT')
            self.assertEqual(marker.read_text(), 'once')
            recovered = self.sandbox('await Ogma.sleep(1); console.log(42);')
            self.assertEqual(recovered.returncode, 0, recovered.stdout + recovered.stderr)
