#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Generate the reference tokens for the MadRust inference runtime.

This is the oracle. It runs the reference implementation of the model on an
ordinary Linux host and writes the token ids the reference produces, so the
Rust runtime can be compared token for token. If the ids differ, the runtime
is wrong: the kernel is not involved in that comparison at all.

The reference runs in float32 on purpose. The shipped weights are bfloat16,
so running the reference in float32 keeps any divergence attributable to the
maths of the runtime rather than to the rounding of the weights.

Usage:
    python3 bench/reference/reference_tokens.py --out reference.json

Needs a virtual environment with transformers and torch:

    python3 -m venv v && v/bin/pip install "transformers>=4.44" torch
"""

import argparse
import json
import sys


def load_model(name: str, revision: str | None):
    """Load the tokenizer and the model in float32, whatever the version."""
    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    tokenizer = AutoTokenizer.from_pretrained(name, revision=revision)
    try:
        model = AutoModelForCausalLM.from_pretrained(
            name, revision=revision, dtype=torch.float32
        )
    except TypeError:
        # transformers before 5.0 spells the argument torch_dtype.
        model = AutoModelForCausalLM.from_pretrained(
            name, revision=revision, torch_dtype=torch.float32
        )
    return tokenizer, model.eval(), torch


def dimensions_of(config) -> dict:
    """The dimensions of the model, wherever the configuration keeps them.

    Some models of the Llama family keep the rotary embedding parameters in a
    nested dictionary instead of in the flat fields, so both places are read
    and the head dimension is derived when it is not stated.
    """
    hidden = int(config.get("hidden_size", 0))
    heads = int(config.get("num_attention_heads", 0))
    rope = config.get("rope_parameters") or {}
    head_dim = int(config.get("head_dim", 0)) or (hidden // heads if heads else 0)
    return {
        "hidden_size": hidden,
        "num_hidden_layers": int(config.get("num_hidden_layers", 0)),
        "num_attention_heads": heads,
        "num_key_value_heads": int(config.get("num_key_value_heads", 0)),
        "head_dim": head_dim,
        "intermediate_size": int(config.get("intermediate_size", 0)),
        "rms_norm_eps": float(config.get("rms_norm_eps", 0.0)),
        "rope_theta": float(rope.get("rope_theta", config.get("rope_theta", 0.0))),
        "rope_interleaved": bool(
            rope.get("rope_interleaved", config.get("rope_interleaved", False))
        ),
        "vocab_size": int(config.get("vocab_size", 0)),
        "tie_word_embeddings": bool(config.get("tie_word_embeddings", False)),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", default="HuggingFaceTB/SmolLM2-135M")
    parser.add_argument(
        "--revision",
        default=None,
        help="commit of the model repository; the resolved one is recorded",
    )
    parser.add_argument("--prompt", default="The capital of France is")
    parser.add_argument("--new-tokens", type=int, default=8)
    parser.add_argument("--out", default="reference.json")
    args = parser.parse_args()

    tokenizer, model, torch = load_model(args.model, args.revision)

    ids = tokenizer(args.prompt, return_tensors="pt").input_ids
    prompt_ids = [int(value) for value in ids[0]]

    with torch.no_grad():
        generated = model.generate(
            ids,
            max_new_tokens=args.new_tokens,
            do_sample=False,
            num_beams=1,
            temperature=None,
            top_p=None,
            top_k=None,
        )
    generated_ids = [int(value) for value in generated[0]]

    # The five best candidates of the first step, to locate a divergence when
    # the ids do not match.
    with torch.no_grad():
        logits = model(ids).logits[0, -1].float()
    top = torch.topk(logits, 5)
    first_step = [
        {"id": int(index), "logit": float(value)}
        for index, value in zip(top.indices, top.values)
    ]

    config = model.config.to_dict()
    dimensions = dimensions_of(config)
    data = {
        "model": args.model,
        "revision": args.revision or "main",
        "prompt": args.prompt,
        "prompt_ids": prompt_ids,
        "generated_ids": generated_ids,
        "new_tokens": generated_ids[len(prompt_ids) :],
        "eos_token_id": int(config.get("eos_token_id", 2)),
        "dtype": "float32",
        "sampling": "greedy",
        "first_step_top5": first_step,
        "dimensions": dimensions,
        "torch": torch.__version__,
    }

    with open(args.out, "w") as handle:
        json.dump(data, handle, indent=2)
        handle.write("\n")

    print("PROMPT_IDS", prompt_ids)
    print("GENERATED_IDS", generated_ids)
    print("NEW_TOKENS", data["new_tokens"])
    print("wrote", args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
