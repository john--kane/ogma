#!/usr/bin/env python3
"""Build the dependency-free GitHub Pages site from recorded runtime evidence."""
import argparse
from datetime import datetime
from html import escape
import json
import math
from pathlib import Path
import re
import shutil

ROOT = Path(__file__).resolve().parents[1]
REPORT = ROOT / 'docs/benchmarks/linux-core-preview.json'


def build(output):
    report = json.loads(REPORT.read_text())
    if (report['schemaVersion'] != 1 or report['kind'] != 'ogma-startup'
        or report['environment']['machine'] != 'x86_64'
        or not report['environment']['os'].startswith('Linux')):
        raise ValueError('landing page requires the recorded Linux x86_64 startup report')
    tokens = {'MEASURED_AT': datetime.fromisoformat(report['measuredAt']).date().isoformat()}
    sample_counts = set()
    for key, prefix in [('cliOnly', 'CLI_ONLY'), ('cliParsing', 'CLI_PARSING'),
                        ('emptyTransformCache', 'EMPTY'), ('populatedTransformCache', 'POPULATED')]:
        result = report['results'][key]
        count = result['samples']
        if type(count) is not int or count <= 0 or len(result['rawMs']) != count:
            raise ValueError('invalid benchmark sample count')
        sample_counts.add(count)
        for field, suffix in [('medianMs', 'MEDIAN'), ('p95Ms', 'P95')]:
            value = result[field]
            if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
                raise ValueError('invalid benchmark statistic')
            tokens[prefix + '_' + suffix] = f'{value:.2f}'
    if len(sample_counts) != 1:
        raise ValueError('landing page sample caption requires equal counts for every mode')
    tokens['SAMPLES'] = str(sample_counts.pop())
    template = (ROOT / 'site/index.html').read_text()
    expected = set(re.findall(r'@@([A-Z0-9_]+)@@', template))
    if expected != set(tokens):
        raise ValueError('site placeholders and evidence fields disagree')
    rendered = re.sub(r'@@([A-Z0-9_]+)@@', lambda match: escape(tokens[match[1]]), template)
    output = output.resolve()
    if output == ROOT or output.is_relative_to(ROOT / 'site'):
        raise ValueError('site output must not overwrite its source')
    output.mkdir(parents=True, exist_ok=True)
    shutil.copytree(ROOT / 'site/assets', output / 'assets', dirs_exist_ok=True)
    for name in ('style.css', 'script.js'):
        shutil.copy2(ROOT / 'site' / name, output / name)
    (output / 'index.html').write_text(rendered, encoding='utf-8')
    (output / 'data').mkdir(exist_ok=True)
    shutil.copy2(REPORT, output / 'data/startup.json')
    (output / '.nojekyll').touch()
    return output


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist/site')
    args = parser.parse_args()
    try:
        print(build(args.output))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f'site build failed: {error}\n')
