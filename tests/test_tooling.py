import json
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts/ogma'

class Tooling(unittest.TestCase):
    def cli(self, *args):
        value = subprocess.run([str(CLI), *map(str, args), '--json'], capture_output=True,
                               text=True, timeout=30, cwd=ROOT)
        return value, json.loads(value.stdout)

    def test_formatter_is_explicit_about_writes(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'main.ts'
            original = 'const answer:number=42;console.log(answer)'
            main.write_text(original)
            check, value = self.cli('fmt', main)
            self.assertNotEqual(check.returncode, 0, value)
            self.assertEqual(main.read_text(), original)
            write, value = self.cli('fmt', main, '--write')
            self.assertEqual(write.returncode, 0, value)
            self.assertNotEqual(main.read_text(), original)
            check, value = self.cli('fmt', main)
            self.assertEqual(check.returncode, 0, value)

    def test_linter_reports_diagnostics(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'main.ts'
            main.write_text('debugger;\n')
            result, value = self.cli('lint', main)
            self.assertNotEqual(result.returncode, 0, value)
            self.assertTrue(any(d['code'].startswith('OGMA_LINT') for d in value['diagnostics']), value)

    def test_checker_is_off_execution_path(self):
        with tempfile.TemporaryDirectory() as folder:
            main = Path(folder) / 'main.ts'
            main.write_text('const value: number = "wrong"; console.log(value);\n')
            result, value = self.cli('check', main)
            self.assertNotEqual(result.returncode, 0, value)
            self.assertTrue(any(d['code'] == 'OGMA_TS2322' for d in value['diagnostics']), value)
            result, value = self.cli('run', main, '--allow-read=' + folder)
            self.assertEqual(result.returncode, 0, value)
            self.assertEqual(value['output'][0]['text'], 'wrong')
            main.write_text('await Ogma.sleep(1); console.log(await Ogma.readTextFile("x"));\n')
            result, value = self.cli('check', main)
            self.assertEqual(result.returncode, 0, value)

    def test_bundle_executes_in_jsc(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'dep.ts').write_text('export const answer: number = 42;\n')
            (root / 'main.ts').write_text('import { answer } from "./dep.ts"; console.log(answer);\n')
            output = root / 'out.js'
            result, value = self.cli('bundle', root / 'main.ts', '--output=' + str(output))
            self.assertEqual(result.returncode, 0, value)
            self.assertTrue(output.exists())
            result, value = self.cli('run', output, '--allow-read=' + folder)
            self.assertEqual(result.returncode, 0, value)
            self.assertEqual(value['output'][0]['text'], '42')

if __name__ == '__main__':
    unittest.main()
