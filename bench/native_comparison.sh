#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Compare the MadRust runtime against the reference implementation of local
# inference, natively, on the same machine, with the same model.
#
# This is the comparison a user would make: not our kernel against another
# kernel, but our program against the program people actually run. Both run
# natively here, so the numbers describe the arithmetic and not an emulator.
#
# The kernel part of the story is measured separately, by the rig in
# bench/run.py, which boots both kernels under identical conditions.
#
# Usage: bench/native_comparison.sh [MODEL_DIRECTORY] [OUTPUT_JSON]
#
# The model directory has to hold the files the runtime opens:
#   model.safetensors  vocab.json  config.json
#
# Needs a Linux host with cmake, a C++ compiler and curl. It builds
# llama.cpp from source, downloads the GGUF checkpoint of the same model and
# runs both programs with the same prompt.

set -eu

MODEL_DIR=${1:-"$HOME/madrust/artifacts/model"}
OUT=${2:-"$MODEL_DIR/native_comparison.json"}
WORK=${WORK:-"$HOME/madrust/native"}
PROMPT="The capital of France is"
TOKENS=${TOKENS:-32}
REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# The same checkpoint the runtime reads, in the format llama.cpp reads.
MODEL_REPO=${MODEL_REPO:-HuggingFaceTB/SmolLM2-135M}
MODEL_REV=${MODEL_REV:-93efa2f097d58c2a74874c7e644dbc9b0cee75a2}
GGUF=${GGUF:-model-f16.gguf}

mkdir -p "$WORK"

echo "== building llama.cpp =="
if [ ! -d "$WORK/llama.cpp" ]; then
    git clone --depth=1 https://github.com/ggml-org/llama.cpp "$WORK/llama.cpp"
fi
cmake -S "$WORK/llama.cpp" -B "$WORK/llama.cpp/build" \
    -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF >/dev/null
cmake --build "$WORK/llama.cpp/build" --config Release -j "$(nproc 2>/dev/null || echo 4)" \
    --target llama-cli >/dev/null
LLAMA="$WORK/llama.cpp/build/bin/llama-cli"

echo "== fetching the checkpoint in the format llama.cpp reads =="
if [ ! -f "$MODEL_DIR/$GGUF" ]; then
    curl -L --fail -o "$MODEL_DIR/$GGUF" \
        "https://huggingface.co/$MODEL_REPO/resolve/$MODEL_REV/$GGUF"
fi

echo "== the MadRust runtime, natively =="
# The runtime is built for the Linux musl target and runs on any Linux host.
RUNTIME="$REPO/madrust-infer/madrust-infer.elf"
if [ ! -x "$RUNTIME" ]; then
    (cd "$REPO" && make infer)
fi
RUNTIME_LOG="$WORK/runtime.log"
MADRUST_INFER_ROOT="$MODEL_DIR" "$RUNTIME" "$TOKENS" > "$RUNTIME_LOG" 2>&1 || true
grep -E '^INFER (tok_per_s|ttft_ms|tokens)' "$RUNTIME_LOG" || true

echo "== llama.cpp, natively =="
LLAMA_LOG="$WORK/llama.log"
"$LLAMA" -m "$MODEL_DIR/$GGUF" -p "$PROMPT" -n "$TOKENS" --temp 0 \
    --no-display-prompt -no-cnv --seed 0 > "$LLAMA_LOG" 2>&1 || true
grep -E 'eval time|tokens per second' "$LLAMA_LOG" || tail -5 "$LLAMA_LOG"

echo "== writing $OUT =="
MADRUST_INFER_ROOT="$MODEL_DIR" "$RUNTIME" "$TOKENS" > "$WORK/runtime.json.log" 2>&1 || true
python3 - "$RUNTIME_LOG" "$LLAMA_LOG" "$OUT" "$MODEL_DIR" "$TOKENS" <<'PY'
import json
import re
import sys

runtime_log, llama_log, out, model_dir, tokens = sys.argv[1:6]


def read(path):
    try:
        return open(path, errors="replace").read()
    except FileNotFoundError:
        return ""


runtime = read(runtime_log)
llama = read(llama_log)


def one(pattern, text, cast=float):
    found = re.search(pattern, text)
    return cast(found.group(1)) if found else None


data = {
    "model": "HuggingFaceTB/SmolLM2-135M",
    "tokens": int(tokens),
    "madrust": {
        "tokens_per_second": one(r"INFER tok_per_s ([0-9.]+)", runtime),
        "time_to_first_token_ms": one(r"INFER ttft_ms ([0-9.]+)", runtime),
    },
    "llamacpp": {
        "tokens_per_second": one(r"([0-9.]+) tokens per second", llama),
        "time_to_first_token_ms": one(r"eval time = ([0-9.]+) ms", llama),
    },
    "notes": [
        "Both programs run natively on the same host with the same checkpoint.",
        "The llama.cpp number is its own evaluation rate, reported by the program.",
        "The MadRust number is the rate of the same forward pass written in Rust.",
    ],
}
with open(out, "w") as handle:
    json.dump(data, handle, indent=2)
    handle.write("\n")
print(json.dumps(data, indent=2))
PY

echo "wrote $OUT"
