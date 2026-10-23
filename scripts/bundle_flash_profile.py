#!/usr/bin/env python3
"""Provision an offline dense Flash candidate from local safetensors and GGUF vocabulary.

APFS clones avoid duplicate physical weight storage when available. The source files
are never edited. All installed bytes are hashed; no model is loaded by this script.
"""
import argparse
import hashlib
import json
import shutil
import struct
import subprocess
from pathlib import Path


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''):
            value.update(block)
    return value.hexdigest()


def extract_vocabulary(source, target):
    def integer(stream, size):
        data = stream.read(size)
        if len(data) != size:
            raise ValueError('Truncated GGUF metadata')
        return int.from_bytes(data, 'little')
    def skip_string(stream):
        size = integer(stream, 8)
        if size > 64*1024*1024:
            raise ValueError('GGUF string limit')
        stream.seek(size, 1)
    def skip_value(stream, kind, depth=0):
        if depth > 4:
            raise ValueError('GGUF metadata nesting limit')
        sizes = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}
        if kind in sizes:
            stream.seek(sizes[kind], 1)
        elif kind == 8:
            skip_string(stream)
        elif kind == 9:
            element, count = integer(stream, 4), integer(stream, 8)
            if count > 10000000:
                raise ValueError('GGUF array limit')
            if element in sizes:
                stream.seek(count*sizes[element], 1)
            else:
                for _ in range(count):
                    skip_value(stream, element, depth+1)
        else:
            raise ValueError('Unknown GGUF metadata type')
    with source.open('rb') as stream:
        if stream.read(4) != b'GGUF' or integer(stream, 4) not in {2, 3}:
            raise ValueError('Unsupported GGUF')
        integer(stream, 8)  # Original tensor count; weights are never read.
        count = integer(stream, 8)
        if not 0 < count <= 100000:
            raise ValueError('GGUF metadata count limit')
        for _ in range(count):
            skip_string(stream)
            skip_value(stream, integer(stream, 4))
            if stream.tell() > 64*1024*1024:
                raise ValueError('GGUF metadata size limit')
        size = stream.tell()
        stream.seek(0)
        data = bytearray(stream.read(size))
        if len(data) != size:
            raise ValueError('Truncated GGUF metadata')
        data[8:16] = struct.pack('<Q', 0)
        data.extend(b'\0'*((-len(data)) % 32))
        target.write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--model', type=Path, required=True)
    parser.add_argument('--vocabulary-source', type=Path, required=True)
    parser.add_argument('--flash-bundle', type=Path, required=True)
    parser.add_argument('--destination', type=Path, required=True)
    parser.add_argument('--name', required=True)
    parser.add_argument('--license-file', type=Path, help='Original model license when a prepared checkpoint omits it')
    parser.add_argument('--weight-bits', type=int, choices=[4, 8], default=4)
    parser.add_argument('--context-limit', type=int, default=8192)
    args = parser.parse_args()
    if args.destination.exists():
        parser.error('Destination must be new')
    if not 256 <= args.context_limit <= 32768:
        parser.error('Context limit must be 256–32768')
    config = json.loads((args.model/'config.json').read_text())
    if config.get('model_type') != 'qwen3_5':
        parser.error('Only a Qwen3.5-architecture dense candidate is supported here')
    if config.get('tie_word_embeddings') or config.get('text_config', {}).get('tie_word_embeddings'):
        parser.error('Pinned dense loader requires a separate lm_head; tied-embedding checkpoints are unsupported')
    text_config = config.get('text_config', config)
    heads, kv_heads = text_config.get('num_attention_heads', 0), text_config.get('num_key_value_heads', 0)
    if not isinstance(heads, int) or not isinstance(kv_heads, int) or kv_heads <= 0 or heads not in {6*kv_heads, 12*kv_heads}:
        parser.error('Pinned dense attention supports only GQA groups 6 or 12')
    license_file=args.license_file or args.model/'LICENSE'
    if not license_file.is_file():parser.error('Supply model LICENSE or --license-file')
    flash = json.loads((args.flash_bundle/'bundle.json').read_text())
    root = args.destination
    root.mkdir(parents=True, mode=0o700)
    model = root/'model'
    model.mkdir(mode=0o700)
    assets = sorted(p for p in args.model.iterdir() if p.is_file() and
                    (p.suffix in {'.json', '.safetensors', '.jinja', '.txt'} or p.name in {'LICENSE', 'NOTICE'}))
    source_hashes={original.name:digest(original.resolve()) for original in assets}
    license_hash=digest(license_file.resolve())
    for original in assets:
        target = model/original.name
        # macOS cp -c uses clonefile, producing independently writable file contents.
        result = subprocess.run(['cp', '-c', str(original.resolve()), str(target)], capture_output=True)
        if result.returncode:
            if shutil.disk_usage(root).free < original.stat().st_size+2*1024**3:
                raise ValueError('Insufficient disk for non-cloned asset copy')
            shutil.copyfile(original, target)
    if not (model/'LICENSE').exists():shutil.copyfile(license_file.resolve(),model/'LICENSE')
    if digest(model/'LICENSE')!=license_hash:raise ValueError('License changed during capture')
    (root/'source-capture.json').write_text(json.dumps(dict(schemaVersion=1,sourceFiles=source_hashes,licenseSha256=license_hash),indent=2)+'\n')
    extract_vocabulary(args.vocabulary_source, root/'tokenizer.gguf')
    profile = dict(name=args.name, flashArtifactId=hashlib.sha256(json.dumps(flash, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest(), architecture='dense',
                   modelPath='model', tokenizerPath='tokenizer.gguf', tokenizerId=digest(root/'tokenizer.gguf'),
                   templatePath='model/chat_template.jinja', templateId=digest(model/'chat_template.jinja'),
                   templateKind='qwen3.8-text-no-thinking-v1', contextLimit=args.context_limit,
                   weightBits=args.weight_bits, license='See model/LICENSE; qualification candidate')
    (root/'profile.json').write_text(json.dumps(profile, indent=2)+'\n')
    artifacts = {}
    for index, path in enumerate(sorted(p for p in root.rglob('*') if p.is_file()), 1):
        print(f'Hashing asset {index}: {path.name}', flush=True)
        artifacts[str(path.relative_to(root))] = digest(path)
    for original in assets:
        if digest(original.resolve())!=source_hashes[original.name] or artifacts['model/'+original.name]!=source_hashes[original.name]:
            raise ValueError('Model inputs changed during capture; retry a new destination')
    if digest(license_file.resolve())!=license_hash:raise ValueError('License changed during capture')
    bundle = dict(schemaVersion=1, kind='generation_profile', entrypoint='profile.json', arguments=[],
                  artifacts=artifacts, manifest=profile)
    (root/'bundle.json').write_text(json.dumps(bundle, indent=2)+'\n')
    print(root)

if __name__ == '__main__':
    main()
