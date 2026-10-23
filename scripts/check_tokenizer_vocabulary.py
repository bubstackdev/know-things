#!/usr/bin/env python3
"""Compare every reference token ID and BPE merge with a local GGUF vocabulary."""
import argparse
import json
import struct
from pathlib import Path
from package_installation import digest


def metadata(path):
    with path.open('rb') as stream:
        def number(fmt):
            size=struct.calcsize(fmt);raw=stream.read(size)
            if len(raw)!=size:raise ValueError('Truncated metadata')
            return struct.unpack('<'+fmt,raw)[0]
        def string():
            size=number('Q')
            if size>64*1024*1024:raise ValueError('String limit')
            raw=stream.read(size)
            if len(raw)!=size:raise ValueError('Truncated string')
            return raw.decode()
        def value(kind,depth=0):
            if depth>4:raise ValueError('Metadata depth limit')
            if kind==8:return string()
            if kind==9:
                element,count=number('I'),number('Q')
                if count>10000000:raise ValueError('Array limit')
                return [value(element,depth+1) for _ in range(count)]
            return number({0:'B',1:'b',2:'H',3:'h',4:'I',5:'i',6:'f',7:'?',10:'Q',11:'q',12:'d'}[kind])
        if stream.read(4)!=b'GGUF' or number('I') not in {2,3}:raise ValueError('Unsupported GGUF')
        number('Q');count=number('Q')
        if not 0<count<=100000:raise ValueError('Metadata count limit')
        result={}
        for _ in range(count):
            key=string();result[key]=value(number('I'))
            if stream.tell()>64*1024*1024:raise ValueError('Metadata limit')
        return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['reference-tokenizer','vocabulary','output']:parser.add_argument('--'+name,type=Path,required=True)
    args=parser.parse_args()
    if args.output.exists():parser.error('Output must be new')
    actual=metadata(args.vocabulary);reference=json.loads(args.reference_tokenizer.read_bytes())
    tokens=actual['tokenizer.ggml.tokens'];vocab=dict(reference['model']['vocab'])
    vocab.update({token['content']:token['id'] for token in reference['added_tokens']})
    mismatches=sum(identifier>=len(tokens) or token!=tokens[identifier] for token,identifier in vocab.items())
    merges=[' '.join(pair) if isinstance(pair,list) else pair for pair in reference['model']['merges']]
    passed=mismatches==0 and merges==actual['tokenizer.ggml.merges']
    report=dict(kind='full-token-id-and-bpe-merge-comparison',scriptSha256=digest(Path(__file__)),referenceTokenizerSha256=digest(args.reference_tokenizer),vocabularySha256=digest(args.vocabulary),referenceTokenCount=len(vocab),ggufTokenCount=len(tokens),tokenIdMismatches=mismatches,referenceMergeCount=len(merges),mergesMatch=merges==actual['tokenizer.ggml.merges'],allPassed=passed,limitations=['Vocabulary/merge comparison does not establish tokenizer preprocessor or special-token behavior; use paired native cases too','No generation, reset, semantic quality, or hardware tier qualification'])
    with args.output.open('x') as stream:json.dump(report,stream,indent=2);stream.write('\n')
    print(json.dumps(report))
    if not passed:raise SystemExit(1)


if __name__=='__main__':main()
