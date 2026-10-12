import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

class Contracts(unittest.TestCase):
    def test_generated_contracts_do_not_drift(self):
        generator = ROOT / 'tools/generate_contracts.py'
        self.assertTrue(generator.exists(), 'contract generator is missing')
        result = subprocess.run(['python3', str(generator), '--check'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

if __name__ == '__main__':
    unittest.main()
