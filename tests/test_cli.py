import json
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts' / ('ogma.exe' if os.name == 'nt' else 'ogma')

class Cli(unittest.TestCase):
    def cli(self, *args):
        return subprocess.run([str(CLI), *args], capture_output=True, text=True, timeout=15)

    def test_literal_output_is_not_markup_or_wrapped(self):
        text = '[red]literal[/] ' + 'é' * 1000
        result = self.cli('eval', 'console.log(' + json.dumps(text) + ');')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, text + '\n')
        self.assertEqual(result.stderr, '')

    def test_json_diagnostic_has_no_terminal_escape_sequences(self):
        result = self.cli('eval', 'throw new Error("[red]literal[/]");', '--json')
        self.assertEqual(result.returncode, 1)
        value = json.loads(result.stdout)
        self.assertIn('[red]literal[/]', value['diagnostics'][0]['message'])
        self.assertEqual(result.stderr, '')
        self.assertNotIn('\x1b', result.stdout)

    def test_plain_diagnostic_preserves_literal_brackets_on_stderr(self):
        result = self.cli('eval', 'throw new Error("[red]literal[/]");')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, '')
        self.assertIn('OGMA_EXECUTION:', result.stderr)
        self.assertIn('[red]literal[/]', result.stderr)
        self.assertNotIn('\x1b', result.stderr)

class Parsing(unittest.TestCase):
    def cli(self, *args):
        return subprocess.run([str(CLI), *args], capture_output=True, text=True, timeout=15)

    def request(self, *args):
        result = self.cli(*args, '--dry-run')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def test_spaced_values_and_repeated_explicit_grants(self):
        value = self.request('run', 'main.ts', '--allow-read', '.', '--allow-read=tests',
                             '--allow-env', 'FOO', '--allow-env=BAR', '--allow-net', 'example.com:443',
                             '--timeout-ms', '1234', '--memory-mb', '64', '--no-cache')
        self.assertEqual(value['permissions']['read'], [str(ROOT), str(ROOT / 'tests')])
        self.assertEqual(value['permissions']['env'], ['FOO', 'BAR'])
        self.assertEqual(value['permissions']['net'], ['example.com:443'])
        self.assertEqual(value['limits']['timeoutMs'], 1234)
        self.assertEqual(value['limits']['maxMemoryMb'], 64)
        self.assertEqual(value['cacheDirectory'], '')

    def test_command_help_generated_without_execution_or_required_entry(self):
        for command in ['run', 'eval', 'bundle', 'install']:
            result = self.cli(command, '--help')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('Usage:', result.stdout)
            self.assertIn(command, result.stdout)
            self.assertNotIn('OGMA_', result.stderr)

    def test_guest_args_are_literal_after_separator(self):
        result = self.cli('run', 'main.ts', '--allow-read=.', '--dry-run', '--',
                          '--allow-net=evil.invalid', '--json', '@response.txt', '[parse]', 'spaces retained')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        value = json.loads(result.stdout)
        self.assertEqual(value['args'], ['--allow-net=evil.invalid', '--json', '@response.txt', '[parse]', 'spaces retained'])
        self.assertEqual(value['permissions']['net'], [])

    def test_exact_source_options_and_disabled_response_files(self):
        source = 'console.log("[parse] @file --json $x");'
        value = self.request('eval', source, '--allow-env=HOME')
        self.assertEqual(value['source'], source)
        result = self.cli('eval', '@response.txt', '--dry-run')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout)['source'], '@response.txt')
        directive = self.request('eval', '[env:OGMA_INJECTED=secret]')
        self.assertEqual(directive['source'], '[env:OGMA_INJECTED=secret]')

    def test_global_flags_version_alias_and_no_cache_precedence(self):
        result = self.cli('--json', 'eval', '1', '--dry-run')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout)['source'], '1')
        alias = self.cli('--version', '--json')
        command = self.cli('version', '--json')
        self.assertEqual(alias.returncode, 0, alias.stdout + alias.stderr)
        self.assertEqual(json.loads(alias.stdout), json.loads(command.stdout))
        for options in [('--no-cache', '--cache-dir=cache'), ('--cache-dir=cache', '--no-cache')]:
            value = self.request('eval', '1', *options)
            self.assertEqual(value['cacheDirectory'], '')

    def test_invalid_inputs_keep_json_codes_and_deny_bare_grants(self):
        for args in [('run',), ('eval', '1', '--allow-read'), ('eval', '1', '--timeout-ms', '0'),
                     ('eval', '1', '--unknown'), ('eval', '1', 'extra'), ('eval', '1', '--json=true'),
                     ('eval', '1', '--allow-env', 'BAD=NAME'),
                     ('eval', '1', '--allow-read=.', '--allow-read'),
                     ('eval', '1', '--allow-env=FOO', '--allow-env'),
                     ('eval', '1', '--allow-read='),
                     ('eval', '1', '--cache-dir'), ('bundle', 'main.ts', '--output')]:
            with self.subTest(args=args):
                result = self.cli(*args, '--json')
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_INVALID_ARGUMENT')
        result = self.cli('unknown-command', '--json')
        self.assertEqual(json.loads(result.stdout)['diagnostics'][0]['code'], 'OGMA_UNSUPPORTED')

if __name__ == '__main__':
    unittest.main()
