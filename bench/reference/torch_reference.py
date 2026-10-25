#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Measure the reference implementation of local inference on this host.

The comparison that matters for the reader is our runtime against a program
people actually run, natively, on the same machine, with the same weights.
This script measures the reference implementation of the model in PyTorch, in
float32, with the same prompt and the same greedy decoding, and reports the
rate.

It reads the same files the Rust runtime reads, so the two programs compute
the same thing: the arithmetic is the only difference.

Usage:
    python3 bench/reference/torch_reference.py --model-dir DIR [--tokens N]
"""

import argparse
import json
import sys
import time


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-dir", required=True)
    parser.add_argument("--prompt", default="The capital of France is")
    parser.add_argument("--tokens", type=int, default=32)
    parser.add_argument(
        "--hub",
        default="HuggingFaceTB/SmolLM2-135M",
        help="where the tokenizer is taken from when the model directory is not enough",
    )
    parser.add_argument("--out", default="")
    args = parser.parse_args()

    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    # The tokenizer needs the vocabulary and the merges together. The model
    # directory may not carry the merges, so the tokenizer is taken from the hub
    # when the local directory is not enough, and the weights always come from
    # the local directory.
    try:
        tokenizer = AutoTokenizer.from_pretrained(args.model_dir)
    except Exception:
        tokenizer = AutoTokenizer.from_pretrained(args.hub)
    try:
        model = AutoModelForCausalLM.from_pretrained(args.model_dir, dtype=torch.float32)
    except TypeError:
        model = AutoModelForCausalLM.from_pretrained(args.model_dir, torch_dtype=torch.float32)
    model = model.eval()

    ids = tokenizer(args.prompt, return_tensors="pt").input_ids

    # The load is not part of the measurement: the rate is the rate of the
    # generated tokens.
    with torch.no_grad():
        started = time.monotonic()
        generated = model.generate(
            ids,
            max_new_tokens=args.tokens,
            do_sample=False,
            num_beams=1,
            temperature=None,
            top_p=None,
            top_k=None,
        )
        elapsed = time.monotonic() - started

    new_tokens = [int(value) for value in generated[0][ids.shape[1] :]]
    rate = len(new_tokens) / elapsed if elapsed > 0 else 0.0

    result = {
        "implementation": "pytorch",
        "dtype": "float32",
        "prompt": args.prompt,
        "tokens": len(new_tokens),
        "seconds": elapsed,
        "tokens_per_second": rate,
        "new_tokens": new_tokens,
        "torch": torch.__version__,
    }

    print("TORCH tokens_per_second", f"{rate:.4f}")
    print("TORCH seconds", f"{elapsed:.4f}")
    print("TORCH new_tokens", " ".join(str(value) for value in new_tokens))

    if args.out:
        with open(args.out, "w") as handle:
            json.dump(result, handle, indent=2)
            handle.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
