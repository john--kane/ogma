#!/usr/bin/env python3
"""Core acceptance and shared CLI parser compatibility; no model processes."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tests'))


def suite():
    loader = unittest.TestLoader()
    tests = unittest.TestSuite()
    for path in sorted((ROOT / 'tests').glob('test_*.py')):
        if not path.name.startswith('test_model'):
            tests.addTests(loader.loadTestsFromName(path.stem))
    # Only dry-run parsing; never launch the model engine/worker/server.
    tests.addTests(loader.loadTestsFromName('test_model_cli.ModelCli'))
    return tests


if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(suite())
    sys.exit(0 if result.wasSuccessful() else 1)
