#!/usr/bin/env python3
"""Explicit, pinned public-model acquisition. Never imported by query handlers."""
import argparse
import hashlib
import json
import os
import urllib.parse
import urllib.request
from pathlib import Path

MODEL = 'sentence-transformers/all-MiniLM-L6-v2'
CANDIDATES = {
    MODEL: ('1110a243fdf4706b3f48f1d95db1a4f5529b4d41', 128*1024*1024),
    'intfloat/e5-base-v2': ('f52bf8ec8c7124536f0efb74aca902b2995e5bcd', 512*1024*1024),
}
FILES = {'config.json', 'tokenizer.json', 'tokenizer_config.json', 'special_tokens_map.json',
         'vocab.txt', 'model.safetensors', 'sentence_bert_config.json', '1_Pooling/config.json',
         'modules.json', 'README.md'}

def fetch_json(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        data = response.read(1024*1024+1)
        if len(data) > 1024*1024:
            raise ValueError('Metadata limit exceeded')
        return json.loads(data)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--destination', type=Path, required=True)
    parser.add_argument('--model', choices=sorted(CANDIDATES), default=MODEL)
    args = parser.parse_args()
    if args.destination.exists():
        parser.error('Destination must be new; incomplete downloads are not active assets')
    model = args.model
    revision, max_asset_bytes = CANDIDATES[model]
    tree = fetch_json(f'https://huggingface.co/api/models/{model}/tree/{revision}?recursive=true')
    entries = {item['path']: item for item in tree if item['path'] in FILES}
    if set(entries) != FILES:
        raise ValueError('Pinned model tree is missing required files')
    args.destination.mkdir(parents=True, mode=0o700)
    receipt = dict(model=model, revision=revision, artifacts={})
    for name, entry in sorted(entries.items()):
        target = args.destination/name
        target.parent.mkdir(parents=True, exist_ok=True)
        size = entry['size']
        if not 0 < size <= max_asset_bytes:
            raise ValueError('Unexpected asset size')
        sha256, git_hash = hashlib.sha256(), hashlib.sha1()
        git_hash.update(f'blob {size}\0'.encode())
        count = 0
        url = f'https://huggingface.co/{model}/resolve/{revision}/{urllib.parse.quote(name)}'
        print(f'Downloading {name} ({size} bytes)', flush=True)
        with urllib.request.urlopen(url, timeout=60) as response, target.open('xb') as output:
            while True:
                block = response.read(65536)
                if not block:
                    break
                count += len(block)
                if count > size:
                    raise ValueError('Asset larger than pinned size')
                sha256.update(block)
                git_hash.update(block)
                output.write(block)
            output.flush()
            os.fsync(output.fileno())
        if count != size:
            raise ValueError('Truncated asset')
        if 'lfs' in entry:
            if sha256.hexdigest() != entry['lfs']['oid']:
                raise ValueError('Weight SHA-256 mismatch')
        elif git_hash.hexdigest() != entry['oid']:
            raise ValueError('Pinned Git object mismatch')
        receipt['artifacts'][name] = sha256.hexdigest()
    (args.destination/'acquisition.json').write_text(json.dumps(receipt, indent=2)+'\n')
    print('Acquired verified pinned assets. Build an embedding bundle before activation.')

if __name__ == '__main__':
    main()
