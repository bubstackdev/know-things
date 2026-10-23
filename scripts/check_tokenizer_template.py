#!/usr/bin/env python3
"""Local paired tokenizer/template diagnostics. Requires tokenizers and Jinja2."""
import argparse
import hashlib
import json
import os
import platform
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/know-things')
    parser.add_argument('--worker-bundle', required=True)
    parser.add_argument('--tokenizer-bundle', required=True)
    parser.add_argument('--reference-model', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    from tokenizers import Tokenizer, __version__ as tokenizer_version
    from jinja2 import __version__ as jinja_version
    from jinja2.sandbox import ImmutableSandboxedEnvironment
    reference = Tokenizer.from_file(str(args.reference_model/'tokenizer.json'))
    raw = subprocess.check_output([args.binary, 'tokenizer-probe', args.worker_bundle, args.tokenizer_bundle], text=True)
    cases = json.loads(raw)
    matches = []
    for case in cases:
        expected = reference.encode(case['text'], add_special_tokens=True).ids
        if case['text'] in {'<think>', '</think>'}:
            assert len(expected) == len(case['tokenIds']) == 1, 'Reasoning delimiters must be single tokens'
        matches.append(dict(inputSha256=hashlib.sha256(case['text'].encode()).hexdigest(),
                            tokenCount=len(expected), matches=expected == case['tokenIds']))
    template_path = args.reference_model/'chat_template.jinja'
    environment = ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True)
    def fail(message):
        raise ValueError(message)
    environment.globals['raise_exception'] = fail
    template = environment.from_string(template_path.read_text())
    templates = []
    for system, user in [('Trusted instructions', 'Hello'), ('  Policy  ', '  Café 🦀\nsecond line.  '), ('Citations only', 'Redis `PersistentCache`\n\nFollow the policy.')]:
        actual = template.render(messages=[dict(role='system', content=system), dict(role='user', content=user)],
                                 add_generation_prompt=True, enable_thinking=False, tools=None)
        expected = f'<|im_start|>system\n{system.strip()}<|im_end|>\n<|im_start|>user\n{user.strip()}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n'
        templates.append(dict(promptSha256=hashlib.sha256(expected.encode()).hexdigest(), matches=actual == expected))
    worker = json.loads((Path(args.worker_bundle)/'bundle.json').read_text())
    summary = dict(kind='paired-tokenizer-template-diagnostics',
                   flashArtifactId=worker['manifest']['flashArtifactId'], workerBuild=worker['manifest']['workerBuild'],
                   referenceTokenizerSha256=hashlib.sha256((args.reference_model/'tokenizer.json').read_bytes()).hexdigest(),
                   referenceTemplateSha256=hashlib.sha256(template_path.read_bytes()).hexdigest(),
                   runtime=dict(python=platform.python_version(), tokenizers=tokenizer_version, jinja2=jinja_version),
                   tokenizerCases=matches, templateCases=templates,
                   allPassed=all(case['matches'] for case in matches+templates),
                   limitations=['Synthetic diagnostic cases; not exhaustive tokenizer equivalence',
                                'No model loaded; generation, accuracy, and hardware tiers remain unqualified'])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps(summary, indent=2))
    if not summary['allPassed']:
        raise SystemExit(1)

if __name__ == '__main__':
    os.environ['PYTHONDONTWRITEBYTECODE'] = '1'
    main()
