#!/usr/bin/env python3
"""Convert AOSP LatinIME's Android 14 Russian word list to Hex's offline swipe model.

Usage: python3 build-russian-swipe.py ru_wordlist.combined.gz path/to/ru.hextm
The source URL and licence are in the adjacent README.
"""
import collections
import gzip
import hashlib
import importlib
import re
import sys
from pathlib import Path

SOURCE_SHA256 = "8ef73a234018d23f5ca9ccc6cb65adecfd5667e87a8b6c6f1ed13ab86abad16e"
# AOSP ProbabilityUtils::encodeRawProbability uses this many frequency units per log2 step.
SCALE = 8.58923700372
WORD = re.compile(r"^ word=([А-Яа-яЁё]{1,32}),f=(\d+)(?:,|$)")


def build(source, output):
    if hashlib.sha256(source.read_bytes()).hexdigest() != SOURCE_SHA256:
        raise ValueError("Unexpected AOSP Russian dictionary")
    weights = collections.Counter()
    titled = collections.Counter()
    blocked = set()
    with gzip.open(source, "rt", encoding="utf-8") as lines:
        for line in lines:
            match = WORD.match(line)
            if not match:
                continue
            spelling, frequency = match.groups()
            word = spelling.lower()
            if ",possibly_offensive=true" in line:
                blocked.add(word)
            elif int(frequency) > 0 and ",not_a_word=true" not in line:
                weight = 2 ** (int(frequency) / SCALE)
                weights[word] += weight
                if spelling.istitle() and len(word) > 1:
                    titled[word] += weight
    for word in blocked:
        weights.pop(word, None)
    # Keep the most frequent forms within the shared HXTM writer's 3.6 MB budget.
    vocab = dict(weights.most_common(190_000))
    writer = importlib.import_module("build-typing-model")
    writer.write_model("ru", vocab, sum(weights.values()), {}, str(output))
    with output.with_suffix(".capitals").open("w", encoding="utf-8", newline="\n") as names:
        for word in sorted(vocab):
            percent = round(100 * titled[word] / vocab[word])
            if percent >= 50:
                names.write(f"{word}\t{percent}\n")


if __name__ == "__main__":
    build(Path(sys.argv[1]), Path(sys.argv[2]))
