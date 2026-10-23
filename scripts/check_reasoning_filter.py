#!/usr/bin/env python3
"""Exercise the native worker's reasoning filter on CPU, without model assets."""
import argparse
import hashlib
import json
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    workers = Path(__file__).resolve().parent.parent/'workers'
    source = r'''
#include "reasoning_filter.h"
#include <cassert>
#include <initializer_list>
#include <string>
void check(std::initializer_list<std::string_view> pieces, std::string_view expected,
           std::size_t visible, std::size_t hidden) {
    ReasoningFilter filter;
    std::string output;
    for (auto piece : pieces) {
        if (filter.accept(piece) == ReasoningFilter::Kind::visible) output += piece;
    }
    assert(output == expected);
    assert(filter.visible_tokens == visible);
    assert(filter.hidden_tokens == hidden);
    assert(filter.visible_tokens + filter.hidden_tokens == pieces.size());
}
int main() {
    check({"Answer ", "[S1]."}, "Answer [S1].", 2, 0);
    check({"<think>", "private reasoning", "</think>", "Answer [S1]."}, "Answer [S1].", 1, 3);
    check({"<think>", "unfinished reasoning"}, "", 0, 2);
    check({"<think>", "first", "<think>", "second", "</think>", "Answer"}, "Answer", 1, 5);
    check({"</think>", "Answer", "<think>", "private", "</think>", " [S1]."}, "Answer [S1].", 2, 4);
    // A new request cannot inherit the preceding request's unfinished reasoning.
    check({"Fresh answer [S1]."}, "Fresh answer [S1].", 1, 0);
}
'''
    with tempfile.TemporaryDirectory(prefix='know-things-filter-') as folder:
        binary = Path(folder)/'check'
        subprocess.run(['c++', '-std=c++20', '-O2', '-Wall', '-Wextra', '-Werror',
                        '-I', str(workers), '-x', 'c++', '-', '-o', str(binary)],
                       input=source, text=True, check=True, timeout=60)
        subprocess.run([str(binary)], check=True, timeout=5)
    summary = dict(kind='native-reasoning-filter-cpu', cases=6, allPassed=True,
                   filterSha256=hashlib.sha256((workers/'reasoning_filter.h').read_bytes()).hexdigest(),
                   limitations=['Synthetic token pieces; profile must qualify single-token delimiters',
                                'No model, tokenizer, GPU, or semantic correctness measurement'])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps(summary))


if __name__ == '__main__':
    main()
