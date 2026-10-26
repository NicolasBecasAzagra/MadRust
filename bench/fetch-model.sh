#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Download the model checkpoint the benchmark serves, and verify every file.
#
# The rig measures a runtime that serves a real checkpoint, so the checkpoint
# has to be somewhere the run can reach. This script fetches it from the model
# hub at a pinned revision and checks the SHA-256 of every file before it is
# used: a weight file that arrived incomplete would produce numbers that mean
# nothing, and the failure would show up much later as a wrong answer.
#
# Usage: bench/fetch-model.sh [OUTPUT_DIRECTORY] [MODEL]
#   OUTPUT_DIRECTORY  where the files are written,
#                     default artifacts/model-instruct
#   MODEL             model repository, default
#                     HuggingFaceTB/SmolLM2-135M-Instruct
#
# The revision is pinned: a moving reference would change the weights under a
# published measurement. The files are the ones a Llama-style runtime needs:
# the weights, the vocabulary, the config of the model, and the tokenizer with
# its merges.
#
# The script needs curl and a SHA-256 tool (sha256sum, or shasum on macOS).

set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$repo/artifacts/model-instruct"}
model=${2:-HuggingFaceTB/SmolLM2-135M-Instruct}

# The revision the measurements were taken with.
revision=12fd25f77366fa6b3b4b768ec3050bf629380bac

# Every file, with the SHA-256 of its bytes.
files="
model.safetensors 5af571cbf074e6d21a03528d2330792e532ca608f24ac70a143f6b369968ab8c
config.json 8eb740e8bbe4cff95ea7b4588d17a2432deb16e8075bc5828ff7ba9be94d982a
vocab.json 82b84012e3add4d01d12ba14442026e49b8cbbaead1f79ecf3d919784f82dc79
tokenizer.json 9ca9acddb6525a194ec8ac7a87f24fbba7232a9a15ffa1af0c1224fcd888e47c
tokenizer_config.json 4ec77d44f62efeb38d7e044a1db318f6a939438425312dfa333b8382dbad98df
merges.txt 0b54e8aa4e53d5383e2e4bc635a56b43f9647f7b13832d5d9ecd8f82dac4f510
"

# The tool that computes the digest, whatever the host calls it.
if command -v sha256sum >/dev/null 2>&1; then
    digest() {
        sha256sum "$1" | cut -d' ' -f1
    }
elif command -v shasum >/dev/null 2>&1; then
    digest() {
        shasum -a 256 "$1" | cut -d' ' -f1
    }
else
    echo "fetch-model: no sha256sum and no shasum on this host" >&2
    exit 1
fi

mkdir -p "$output"

# The files are read one line at a time: name, then digest.
echo "$files" | while read -r name expected; do
    [ -n "$name" ] || continue
    target="$output/$name"
    url="https://huggingface.co/$model/resolve/$revision/$name"

    if [ -f "$target" ]; then
        current=$(digest "$target")
        if [ "$current" = "$expected" ]; then
            echo "fetch-model: $name is already there and its digest matches"
            continue
        fi
        echo "fetch-model: $name has the wrong digest, downloading it again" >&2
        rm -f "$target"
    fi

    echo "fetch-model: downloading $name"
    partial="$target.partial"
    rm -f "$partial"
    if ! curl --fail --location --silent --show-error --output "$partial" "$url"; then
        echo "fetch-model: the download of $name failed" >&2
        rm -f "$partial"
        exit 1
    fi

    current=$(digest "$partial")
    if [ "$current" != "$expected" ]; then
        echo "fetch-model: $name arrived with digest $current" >&2
        echo "fetch-model: the expected digest is $expected" >&2
        rm -f "$partial"
        exit 1
    fi
    mv "$partial" "$target"
    echo "fetch-model: $name is here and its digest matches"
done

echo "fetch-model: wrote $output"
echo "fetch-model: model $model at revision $revision"
echo "fetch-model: now run the rig with --initrd over an initramfs that carries it"
