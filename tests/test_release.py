import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseTests(unittest.TestCase):
    def command(self, script, *args, env=None):
        return subprocess.run(['python3', str(ROOT / 'tools' / script), *map(str, args)],
                              capture_output=True, text=True, env=env)

    def test_reproducible_archive_and_unsigned_opt_in(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / 'input'
            source.mkdir()
            (source / 'ogma').write_text('preview binary')
            (source / 'ogma').chmod(0o755)
            for output in (root / 'first', root / 'second'):
                result = self.command('release.py', '--input', source, '--output', output,
                                      '--name', 'ogma-test')
                self.assertEqual(result.returncode, 0, result.stderr)
            first = root / 'first'
            self.assertEqual((first / 'ogma-test.tar.gz').read_bytes(),
                             (root / 'second' / 'ogma-test.tar.gz').read_bytes())
            manifest = first / 'ogma-test.manifest.json'
            metadata = json.loads(manifest.read_text())
            self.assertEqual(metadata['sha256'], hashlib.sha256(
                (first / 'ogma-test.tar.gz').read_bytes()).hexdigest())
            self.assertNotEqual(self.command('verify_release.py', manifest).returncode, 0)
            verified = self.command('verify_release.py', manifest, '--allow-unsigned')
            self.assertEqual(verified.returncode, 0, verified.stderr)
            with tarfile.open(first / 'ogma-test.tar.gz') as archive:
                self.assertEqual(archive.getnames(), ['ogma-test', 'ogma-test/ogma'])
                self.assertEqual(archive.getmember('ogma-test/ogma').mode, 0o755)

    def test_ed25519_signature_and_tamper_rejection(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / 'input'
            source.mkdir()
            (source / 'ogma').write_bytes(b'preview')
            private = root / 'test-only.pem'
            public = root / 'test-only.pub'
            subprocess.run(['openssl', 'genpkey', '-algorithm', 'ED25519', '-out', str(private)], check=True)
            subprocess.run(['openssl', 'pkey', '-in', str(private), '-pubout', '-out', str(public)], check=True)
            output = root / 'output'
            result = self.command('release.py', '--input', source, '--output', output,
                                  '--name', 'ogma-test', '--sign-key', private)
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = output / 'ogma-test.manifest.json'
            verified = self.command('verify_release.py', manifest, '--public-key', public)
            self.assertEqual(verified.returncode, 0, verified.stderr)
            other = root / 'wrong-key.pem'
            other_public = root / 'wrong-key.pub'
            subprocess.run(['openssl', 'genpkey', '-algorithm', 'ED25519', '-out', str(other)], check=True)
            subprocess.run(['openssl', 'pkey', '-in', str(other), '-pubout', '-out', str(other_public)], check=True)
            self.assertNotEqual(self.command('verify_release.py', manifest, '--public-key', other_public).returncode, 0)
            original = manifest.read_bytes()
            manifest.write_bytes(original + b' ')
            self.assertNotEqual(self.command('verify_release.py', manifest, '--public-key', public).returncode, 0)
            manifest.write_bytes(original)
            downgraded = json.loads(original)
            downgraded['signatureAlgorithm'] = 'unsigned'
            manifest.write_text(json.dumps(downgraded))
            self.assertNotEqual(self.command('verify_release.py', manifest, '--public-key', public,
                                            '--allow-unsigned').returncode, 0)
            manifest.write_bytes(original)
            archive = output / 'ogma-test.tar.gz'
            archive.write_bytes(archive.read_bytes() + b'tamper')
            self.assertNotEqual(self.command('verify_release.py', manifest, '--public-key', public).returncode, 0)

    def test_verified_archive_snapshot_survives_source_replacement(self):
        import sys
        sys.path.insert(0, str(ROOT / 'tools'))
        from release import package
        from verify_release import verify
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / 'input'; source.mkdir()
            (source / 'ogma').write_bytes(b'trusted bytes')
            manifest = package(source, root / 'release', 'ogma-test')
            snapshots = root / 'snapshots'; snapshots.mkdir(mode=0o700)
            verified = verify(manifest, allow_unsigned=True, snapshot_directory=snapshots)
            original = manifest.parent / 'ogma-test.tar.gz'
            expected = original.read_bytes()
            original.write_bytes(b'changed after verification')
            self.assertEqual(verified.read_bytes(), expected)
            self.assertEqual(verified.parent, snapshots)

    def test_refuses_symlinks_and_path_escape_names(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / 'input'
            source.mkdir()
            (source / 'escape').symlink_to('/etc/passwd')
            self.assertNotEqual(self.command('release.py', '--input', source,
                '--output', root / 'output', '--name', 'ogma-test').returncode, 0)
            (source / 'escape').unlink()
            (source / 'ogma').write_bytes(b'preview')
            self.assertNotEqual(self.command('release.py', '--input', source,
                '--output', root / 'output', '--name', '../escape').returncode, 0)

    def test_signature_authenticates_exact_metadata_snapshot(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / 'input'
            source.mkdir()
            (source / 'ogma').write_bytes(b'trusted preview')
            private, public = root / 'test.pem', root / 'test.pub'
            real_openssl = shutil.which('openssl')
            subprocess.run([real_openssl, 'genpkey', '-algorithm', 'ED25519', '-out', str(private)], check=True)
            subprocess.run([real_openssl, 'pkey', '-in', str(private), '-pubout', '-out', str(public)], check=True)
            output = root / 'output'
            result = self.command('release.py', '--input', source, '--output', output,
                                  '--name', 'ogma-test', '--sign-key', private)
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = output / 'ogma-test.manifest.json'
            authentic = manifest.read_bytes()
            malicious = output / 'malicious.tar.gz'
            malicious.write_bytes(b'unauthenticated package')
            metadata = json.loads(authentic)
            metadata.update(artifact=malicious.name, sha256=hashlib.sha256(malicious.read_bytes()).hexdigest(),
                            size=malicious.stat().st_size)
            manifest.write_text(json.dumps(metadata))
            wrappers = root / 'bin'
            wrappers.mkdir()
            wrapper = wrappers / 'openssl'
            wrapper.write_text('#!/usr/bin/env python3\nimport os, sys\nfrom pathlib import Path\n'
                'if "-verify" in sys.argv:\n'
                f'    Path({str(manifest)!r}).write_bytes({authentic!r})\n'
                f'os.execv({real_openssl!r}, [{real_openssl!r}, *sys.argv[1:]])\n')
            wrapper.chmod(0o755)
            environment = dict(os.environ, PATH=str(wrappers) + os.pathsep + os.environ['PATH'])
            result = self.command('verify_release.py', manifest, '--public-key', public, env=environment)
            self.assertNotEqual(result.returncode, 0, 'accepted metadata not authenticated by signature')
