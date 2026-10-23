#!/usr/bin/env python3
"""Extract and pin a tokenizer-only GGUF without reading model tensor bytes."""
import argparse
import json
from pathlib import Path
from bundle_flash_profile import digest, extract_vocabulary

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--destination', type=Path, required=True)
    args = parser.parse_args()
    args.destination.mkdir(parents=True, mode=0o700, exist_ok=False)
    target = args.destination/'tokenizer.gguf'
    extract_vocabulary(args.source, target)
    fingerprint = digest(target)
    bundle = dict(schemaVersion=1, kind='tokenizer', entrypoint='tokenizer.gguf', arguments=[],
                  artifacts={'tokenizer.gguf': fingerprint}, manifest={'tokenizerId': fingerprint})
    (args.destination/'bundle.json').write_text(json.dumps(bundle, indent=2)+'\n')
    print(json.dumps(dict(bytes=target.stat().st_size, sha256=fingerprint)))

if __name__ == '__main__':
    main()
