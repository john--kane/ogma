"""Local registry acceptance tests: no public network or package code execution."""
import base64
import hashlib
import http.server
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts' / 'ogma'


class Registry:
    def __init__(self):
        self.versions = {}
        self.tarballs = {}
        self.requests = []
        registry = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                from urllib.parse import unquote
                registry.requests.append(self.path)
                key = unquote(self.path.lstrip('/'))
                if key.startswith('tar/') and key in registry.tarballs:
                    data = registry.tarballs[key]
                elif key in registry.versions:
                    data = json.dumps({'versions': registry.versions[key]}).encode()
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def package(self, name='example', version='1.0.0', code='export const answer = 42;',
                dependencies=None, extra=None, entries=None, wrong_integrity=False):
        manifest = {'name': name, 'version': version, 'type': 'module',
                    'exports': './index.js', 'dependencies': dependencies or {}}
        manifest.update(extra or {})
        files = [('package/package.json', json.dumps(manifest).encode()),
                 ('package/index.js', code.encode())]
        files += entries or []
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode='w:gz') as archive:
            for path, data in files:
                info = tarfile.TarInfo(path)
                if isinstance(data, tuple):
                    info.type, info.linkname = data
                    archive.addfile(info)
                else:
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
        tarball = raw.getvalue()
        key = 'tar/' + name.replace('/', '-') + '-' + version + '.tgz'
        self.tarballs[key] = tarball
        integrity = base64.b64encode(hashlib.sha512(b'wrong' if wrong_integrity else tarball).digest()).decode()
        metadata = dict(manifest, dist={'integrity': 'sha512-' + integrity,
                                      'tarball': self.url + '/' + key})
        self.versions.setdefault(name, {})[version] = metadata


class Install(unittest.TestCase):
    def setUp(self):
        self.assertTrue(CLI.is_file(), 'Build the NativeAOT CLI first')
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name)
        self.registry = Registry()
        self.addCleanup(self.registry.close)
        self.addCleanup(self.temp.cleanup)

    def manifest(self, dependencies):
        (self.project / 'package.json').write_text(json.dumps({'dependencies': dependencies}))

    def install(self, *flags):
        return subprocess.run([str(CLI), 'install', str(self.project),
                               '--registry=' + self.registry.url,
                               '--store=' + str(self.project / 'store'), '--json', *flags],
                              text=True, capture_output=True, cwd=ROOT, timeout=20)

    def success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def failure(self, result, text):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(text.lower(), (result.stdout + result.stderr).lower())

    def test_locked_offline_integrity_and_import(self):
        self.registry.package()
        self.manifest({'example': '^1.0.0'})
        self.success(self.install())
        lock = (self.project / 'ogma.lock.json').read_bytes()
        original = (self.project / 'node_modules/example/index.js').read_bytes()
        shutil.rmtree(self.project / 'node_modules')
        count = len(self.registry.requests)
        self.success(self.install('--locked'))
        self.assertEqual(len(self.registry.requests), count, 'Locked install performed a network request')
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), lock)
        self.assertEqual((self.project / 'node_modules/example/index.js').read_bytes(), original)
        (self.project / 'main.ts').write_text('import {answer} from "example"; console.log(answer);')
        result = subprocess.run([str(CLI), 'run', str(self.project / 'main.ts'),
                                 '--allow-read=' + str(self.project), '--json'],
                                capture_output=True, text=True, timeout=15)
        self.success(result)
        self.assertIn('42', result.stdout)
        blob = next((self.project / 'store').glob('*.tgz'))
        blob.write_bytes(b'corruption')
        self.failure(self.install('--locked'), 'integrity mismatch')
        self.assertEqual((self.project / 'node_modules/example/index.js').read_bytes(), original)
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), lock)

    def test_nested_versions_scoped_exports_and_determinism(self):
        self.registry.package('leaf', '1.0.0', 'export const value = 1;')
        self.registry.package('leaf', '1.1.0', 'export const value = 11;')
        self.registry.package('leaf', '2.0.0', 'export const value = 2;')
        self.registry.package('left', dependencies={'leaf': '~1.0.0'},
                              code='import {value} from "leaf"; export const left = value;')
        self.registry.package('@demo/right', dependencies={'leaf': '^2.0.0'},
                              code='import {value} from "leaf"; export const right = value;',
                              extra={'exports': {'.': {'import': './index.js'}, './value': './index.js'}})
        self.manifest({'left': '1.0.0', '@demo/right': '^1.0.0'})
        self.success(self.install())
        lock = (self.project / 'ogma.lock.json').read_bytes()
        self.success(self.install())
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), lock)
        (self.project / 'main.ts').write_text('import {left} from "left"; '
             'import {right} from "@demo/right/value"; console.log(left + right);')
        result = subprocess.run([str(CLI), 'run', str(self.project / 'main.ts'),
                                 '--allow-read=' + str(self.project), '--json'],
                                capture_output=True, text=True, timeout=15)
        self.success(result)
        self.assertIn('"3"', result.stdout)

    def test_integrity_failure_preserves_install_and_lock(self):
        self.registry.package()
        self.manifest({'example': '1.0.0'})
        self.success(self.install())
        old_lock = (self.project / 'ogma.lock.json').read_bytes()
        self.registry.package('example', '2.0.0', wrong_integrity=True)
        self.manifest({'example': '^2.0.0'})
        self.failure(self.install(), 'integrity mismatch')
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), old_lock)
        self.assertTrue((self.project / 'node_modules/example/index.js').is_file())

    def test_lifecycle_native_and_commonjs_are_rejected(self):
        cases = [({'scripts': {'postinstall': 'touch SHOULD_NOT_EXIST'}}, [], 'scripts'),
                 ({'gypfile': True}, [], 'native'),
                 ({'type': 'commonjs'}, [], 'commonjs'),
                 ({}, [('package/addon.node', b'native')], 'native')]
        for index, (extra, entries, error) in enumerate(cases):
            with self.subTest(error=error):
                self.registry.package('bad', f'1.0.{index}', extra=extra, entries=entries)
                self.manifest({'bad': f'1.0.{index}'})
                self.failure(self.install(), error)
                self.assertFalse((self.project / 'SHOULD_NOT_EXIST').exists())
                self.assertFalse((self.project / 'node_modules').exists())

    def test_hostile_archives_are_rejected_without_escape(self):
        entries = [('package/../../ESCAPED', b'bad'),
                   ('/tmp/OGMA_ESCAPED', b'bad'),
                   ('package/link', (tarfile.SYMTYPE, '../../ESCAPED')),
                   ('package/link', (tarfile.LNKTYPE, 'package/index.js'))]
        for index, entry in enumerate(entries):
            with self.subTest(entry=entry[0]):
                self.registry.package('bad', f'1.0.{index}', entries=[entry])
                self.manifest({'bad': f'1.0.{index}'})
                result = self.install()
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse((self.project / 'node_modules').exists())
                self.assertFalse((self.project.parent / 'ESCAPED').exists())

    def test_package_metadata_requires_guest_read_permission(self):
        self.registry.package()
        self.manifest({'example': '1.0.0'})
        self.success(self.install())
        main = self.project / 'main.ts'
        main.write_text('await import("example");')
        result = subprocess.run([str(CLI), 'run', str(main),
                                 '--allow-read=' + str(main), '--json'],
                                capture_output=True, text=True, timeout=15)
        self.failure(result, 'OGMA_PERMISSION_DENIED')

    def test_failed_lock_publication_rolls_back_complete_modules(self):
        self.registry.package()
        self.manifest({'example': '1.0.0'})
        self.success(self.install())
        old = (self.project / 'node_modules/example/index.js').read_bytes()
        (self.project / 'ogma.lock.json').unlink()
        (self.project / 'ogma.lock.json').mkdir()
        self.registry.package('example', '2.0.0', 'export const answer = 99;')
        self.manifest({'example': '2.0.0'})
        result = self.install()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.project / 'node_modules/example/index.js').read_bytes(), old)
        self.assertTrue((self.project / 'ogma.lock.json').is_dir())
        self.assertEqual(list(self.project.glob('.ogma-install-*')), [])

    def test_crash_after_module_exchange_fails_closed_until_manual_recovery(self):
        self.registry.package()
        self.manifest({'example': '1.0.0'})
        self.success(self.install())
        old_lock = (self.project / 'ogma.lock.json').read_bytes()
        old_code = (self.project / 'node_modules/example/index.js').read_bytes()
        # Materialize exactly the state left by a crash after directory exchange
        # and before lock publication: old modules in staging, new modules live.
        stage = self.project / '.ogma-install-crash-fixture'
        stage.mkdir()
        (stage / 'previous-ogma.lock.json').write_bytes(old_lock)
        (self.project / 'node_modules').rename(stage / 'node_modules')
        package = self.project / 'node_modules/example'
        package.mkdir(parents=True)
        (package / 'package.json').write_text(json.dumps({'name': 'example', 'version': '2.0.0',
                                                        'type': 'module', 'exports': './index.js'}))
        new_code = b'export const answer = 99;'
        (package / 'index.js').write_bytes(new_code)
        journal = self.project / '.ogma-install-journal.json'
        journal_bytes = json.dumps({'version': 1, 'stage': stage.name, 'locked': False,
                                    'hadModules': True, 'previousLockPresent': True,
                                    'previousLockSha512': hashlib.sha512(old_lock).hexdigest(),
                                    'newLockSha512': hashlib.sha512(b'new candidate').hexdigest()}).encode()
        journal.write_bytes(journal_bytes)
        before_requests = len(self.registry.requests)
        for flags in [(), ('--locked',)]:
            result = self.install(*flags)
            self.failure(result, 'Interrupted npm installation')
            self.assertIn('restore a matching', result.stdout + result.stderr)
        self.assertEqual(len(self.registry.requests), before_requests)
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), old_lock)
        self.assertEqual((package / 'index.js').read_bytes(), new_code)
        self.assertEqual((stage / 'node_modules/example/index.js').read_bytes(), old_code)
        self.assertEqual(journal.read_bytes(), journal_bytes)
        main = self.project / 'main.ts'
        main.write_text('await import("example");')
        result = subprocess.run([str(CLI), 'run', str(main),
                                 '--allow-read=' + str(self.project), '--json'],
                                capture_output=True, text=True, timeout=15)
        self.failure(result, 'Interrupted npm installation')
        # Recovery requires an explicit operator decision; the installer never
        # chooses which generation to retain and never replays package execution.
        shutil.rmtree(self.project / 'node_modules')
        (stage / 'node_modules').rename(self.project / 'node_modules')
        journal.unlink()
        shutil.rmtree(stage)
        self.success(self.install('--locked'))
        self.assertEqual((self.project / 'node_modules/example/index.js').read_bytes(), old_code)
        self.assertEqual((self.project / 'ogma.lock.json').read_bytes(), old_lock)
        self.assertFalse(journal.exists())

    def test_changed_manifest_missing_store_and_unsupported_ranges_fail_locked(self):
        self.registry.package()
        self.manifest({'example': '1.0.0'})
        self.success(self.install())
        shutil.rmtree(self.project / 'store')
        self.failure(self.install('--locked'), 'store')
        self.manifest({'example': '^1.0.0'})
        self.failure(self.install('--locked'), 'lockfile')
        for version in ['latest', '*', 'git+https://example.com/pkg', 'file:../pkg', '>=1.0.0']:
            self.manifest({'example': version})
            self.failure(self.install(), 'exact')


if __name__ == '__main__':
    unittest.main()
