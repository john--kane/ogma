"""Archive trust boundary; installed-runtime smoke is an explicit release gate."""
import io
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import verify_preview


class PreviewArchive(unittest.TestCase):
    def archive(self, root, entries):
        path = root / 'preview.tar.gz'
        with tarfile.open(path, 'w:gz') as archive:
            for name, kind, body in entries:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.mode = 0o7777
                if kind == tarfile.REGTYPE:
                    info.size = len(body)
                    archive.addfile(info, io.BytesIO(body))
                else:
                    info.linkname = body.decode()
                    archive.addfile(info)
        return path

    def test_refuses_traversal_links_duplicates_and_multiple_roots(self):
        cases = [
            [('preview/../../escape', tarfile.REGTYPE, b'bad')],
            [('/absolute/file', tarfile.REGTYPE, b'bad')],
            [('preview/link', tarfile.SYMTYPE, b'/etc/passwd')],
            [('preview/link', tarfile.LNKTYPE, b'preview/ogma')],
            [('preview/fifo', tarfile.FIFOTYPE, b'')],
            [('preview/file', tarfile.REGTYPE, b'first'), ('preview/file', tarfile.REGTYPE, b'second')],
            [('one/ogma', tarfile.REGTYPE, b'a'), ('two/worker', tarfile.REGTYPE, b'b')],
            [('preview\\escape', tarfile.REGTYPE, b'bad')],
        ]
        for entries in cases:
            with self.subTest(entries=entries), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                archive = self.archive(root, entries)
                with self.assertRaises(ValueError):
                    verify_preview.extract(archive, root / 'install')
                self.assertFalse((root / 'escape').exists())

    def test_regular_files_extract_without_privileged_modes(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            archive = self.archive(root, [('preview/ogma', tarfile.REGTYPE, b'cli'),
                                          ('preview/docs/readme', tarfile.REGTYPE, b'text')])
            installed = verify_preview.extract(archive, root / 'install')
            self.assertEqual(installed, root / 'install/preview')
            self.assertEqual((installed / 'ogma').read_bytes(), b'cli')
            self.assertEqual((installed / 'ogma').stat().st_mode & 0o7777, 0o755)

    def test_existing_install_directory_and_byte_limits_fail_closed(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            archive = self.archive(root, [('preview/file', tarfile.REGTYPE, b'12345')])
            destination = root / 'existing'
            destination.mkdir()
            with self.assertRaises(ValueError): verify_preview.extract(archive, destination)
            with self.assertRaises(ValueError): verify_preview.extract(archive, root / 'bounded', max_bytes=4)

    def test_layout_requires_runtime_tools_contracts_and_bindings(self):
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaises(ValueError): verify_preview.check_layout(Path(folder))
