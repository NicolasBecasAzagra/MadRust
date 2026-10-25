#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Compare the MadRust runtime against the reference implementations of local
# inference, natively, on the same machine, with the same weights.
#
# This is the comparison a user would make: not our kernel against another
# kernel, but our program against the programs people actually run. Both run
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
# Needs a Linux host with cmake, a C++ compiler, curl, git and a Python
# environment with torch and transformers. The reference in PyTorch is always
# measured; llama.cpp is measured too when its GGUF conversion of the same
# checkpoint can be built, which needs the converter that comes with it.
#
# The GGUF is optional on purpose: it is a conversion of the same weights,
# and a quantized one would not be the same arithmetic. The comparison that
# isolates the arithmetic is against the reference in float32.

set -eu

MODEL_DIR=${1:-"$HOME/madrust/artifacts/model"}
OUT=${2:-"$MODEL_DIR/native_comparison.json"}
WORK=${WORK:-"$HOME/madrust/native"}
PROMPT="The capital of France is"
TOKENS=${TOKENS:-32}
REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

mkdir -p "$WORK"

# Preflight: say what is missing instead of failing in the middle of a build.
missing=""
for tool in git cmake curl python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        missing="$missing $tool"
    fi
done
if [ -n "$missing" ]; then
    echo "native_comparison: missing tools:$missing" >&2
    echo "  sudo apt update && sudo apt install -y cmake build-essential curl git python3-venv" >&2
    exit 1
fi
if ! python3 -c "import torch, transformers" >/dev/null 2>&1; then
    echo "native_comparison: torch and transformers are needed for the reference" >&2
    echo "  python3 -m venv v && v/bin/pip install torch transformers" >&2
    exit 1
fi

echo "== building llama.cpp =="
if [ ! -d "$WORK/llama.cpp" ]; then
    git clone --depth=1 https://github.com/ggml-org/llama.cpp "$WORK/llama.cpp"
fi
cmake -S "$WORK/llama.cpp" -B "$WORK/llama.cpp/build" \
    -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF >/dev/null
cmake --build "$WORK/llama.cpp/build" --config Release -j "$(nproc 2>/dev/null || echo 4)" \
    --target llama-cli >/dev/null
LLAMA="$WORK/llama.cpp/build/bin/llama-cli"

echo "== the MadRust runtime, natively =="
RUNTIME="$REPO/madrust-infer/madrust-infer.elf"
if [ ! -x "$RUNTIME" ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "native_comparison: cargo is needed to build the runtime" >&2
        echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal" >&2
        echo "  then run 'make infer' in $REPO" >&2
        exit 1
    fi
    (cd "$REPO" && make infer)
fi
RUNTIME_LOG="$WORK/runtime.log"
MADRUST_INFER_ROOT="$MODEL_DIR" "$RUNTIME" "$TOKENS" > "$RUNTIME_LOG" 2>&1 || true
grep -E '^INFER (tok_per_s|ttft_ms|tokens)' "$RUNTIME_LOG" || true

echo "== the reference in PyTorch, natively =="
TORCH_LOG="$WORK/torch.log"
python3 "$REPO/bench/reference/torch_reference.py" --model-dir "$MODEL_DIR" \
    --prompt "$PROMPT" --tokens "$TOKENS" --out "$WORK/torch.json" | tee "$TORCH_LOG"

echo "== llama.cpp, natively, when its checkpoint can be converted =="
GGUF="$MODEL_DIR/smollm2-135m-f16.gguf"
LLAMA_LOG="$WORK/llama.log"
if [ ! -f "$GGUF" ] && [ -f "$WORK/llama.cpp/convert_hf_to_gguf.py" ]; then
    echo "converting the checkpoint, this takes a while"
    (cd "$WORK" && python3 llama.cpp/convert_hf_to_gguf.py "$MODEL_DIR" \
        --outfile "$GGUF" --outtype f16 >/dev/null 2>&1) || true
fi
if [ -f "$GGUF" ]; then
    "$LLAMA" -m "$GGUF" -p "$PROMPT" -n "$TOKENS" --temp 0 \
        --no-display-prompt -no-cnv --seed 0 > "$LLAMA_LOG" 2>&1 || true
    grep -E 'tokens per second|eval time' "$LLAMA_LOG" || tail -3 "$LLAMA_LOG"
else
    echo "no GGUF available, llama.cpp is not measured"
    : > "$LLAMA_LOG"
fi

echo "== writing $OUT =="
python3 - "$RUNTIME_LOG" "$WORK/torch.json" "$LLAMA_LOG" "$OUT" "$TOKENS" <<'PY'
import json
import re
import sys

runtime_log, torch_json, llama_log, out, tokens = sys.argv[1:6]


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


try:
    torch = json.load(open(torch_json))
except Exception:
    torch = {}

data = {
    "tokens": int(tokens),
    "madrust": {
        "tokens_per_second": one(r"INFER tok_per_s ([0-9.]+)", runtime),
        "time_to_first_token_ms": one(r"INFER ttft_ms ([0-9.]+)", runtime),
    },
    "pytorch": {
        "tokens_per_second": torch.get("tokens_per_second"),
        "seconds": torch.get("seconds"),
        "new_tokens": torch.get("new_tokens"),
    },
    "llamacpp": {
        "tokens_per_second": one(r"([0-9.]+) tokens per second", llama),
        "time_to_first_token_ms": one(r"eval time = ([0-9.]+) ms", llama),
    },
    "notes": [
        "Every program runs natively on this host, with the same weights.",
        "The PyTorch number is the reference in float32, which is the same arithmetic the Rust runtime does.",
        "The llama.cpp number is optional and is a converted checkpoint, so it is a different precision.",
        "The kernel part of the story is measured by bench/run.py, which boots both kernels.",
    ],
}
with open(out, "w") as handle:
    json.dump(data, handle, indent=2)
    handle.write("\n")
print(json.dumps(data, indent=2))
PY

echo "wrote $OUT"
