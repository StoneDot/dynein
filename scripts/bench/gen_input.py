#!/usr/bin/env python3
"""Deterministic input generator for the benchmark item mixes (benchmark-plan.md §3).

Emits JSON Lines (one item object per line) suitable for `dy import -f jsonl`.
Items carry a string partition key "pk" ("item-00000001", ...) and a "data"
payload string. The seed and the item index are embedded in every payload so
items are globally unique, and the whole file is a pure function of
(--mix, --items, --seed): same arguments => byte-identical file.

Mixes:
  uniform-small  ~110 B per serialized item (~1 WCU each)
  mixed          90% small + 10% ~20 KB (~20 WCU) items, interleaved by the
                 seeded RNG (per-item Bernoulli draw, so the large share is
                 ~10%, not exactly 10%)
  uniform-large  size drawn uniformly from 20,000..50,000 bytes per item

The writer streams line by line (constant memory), so quota-scale inputs
(~16M items, multi-GB) are fine.
"""

import argparse
import json
import random
import sys

SMALL_TARGET_BYTES = 110
LARGE_TARGET_BYTES = 20_000
LARGE_MIN_BYTES = 20_000
LARGE_MAX_BYTES = 50_000
MIXED_LARGE_RATIO = 0.10

# Average WCU per item, used by run_cells.sh/launch.sh to size cells.
AVG_WCU = {"uniform-small": 1.0, "mixed": 2.9, "uniform-large": 35.0}

PAD_ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789"


def make_line(index: int, seed: int, target_bytes: int, rng: random.Random) -> str:
    """Build one serialized JSON line of approximately target_bytes bytes."""
    pk = f"item-{index:08d}"
    # Unique, seed-dependent prefix; padding char varies per item (seeded)
    # so payloads differ between seeds even at identical sizes.
    prefix = f"s{seed}-i{index}-"
    skeleton = json.dumps({"pk": pk, "data": prefix}, separators=(",", ":"))
    pad_len = max(0, target_bytes - len(skeleton) - 1)  # -1 for the newline
    pad_char = PAD_ALPHABET[rng.randrange(len(PAD_ALPHABET))]
    item = {"pk": pk, "data": prefix + pad_char * pad_len}
    return json.dumps(item, separators=(",", ":"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--mix",
        required=True,
        choices=["uniform-small", "mixed", "uniform-large"],
    )
    parser.add_argument("--items", type=int, required=True, help="number of items")
    parser.add_argument("--seed", type=int, required=True, help="RNG seed")
    parser.add_argument("--out", required=True, help="output file path")
    parser.add_argument(
        "--format",
        default="jsonl",
        choices=["jsonl"],
        help="output format (only jsonl is supported)",
    )
    args = parser.parse_args()

    if args.items <= 0:
        parser.error("--items must be positive")

    rng = random.Random(args.seed)
    written_bytes = 0
    with open(args.out, "w", encoding="ascii") as f:
        for i in range(args.items):
            if args.mix == "uniform-small":
                target = SMALL_TARGET_BYTES
            elif args.mix == "mixed":
                if rng.random() < MIXED_LARGE_RATIO:
                    target = LARGE_TARGET_BYTES
                else:
                    target = SMALL_TARGET_BYTES
            else:  # uniform-large
                target = rng.randint(LARGE_MIN_BYTES, LARGE_MAX_BYTES)
            line = make_line(i, args.seed, target, rng)
            f.write(line)
            f.write("\n")
            written_bytes += len(line) + 1

    print(
        f"wrote {args.items} items, {written_bytes} bytes "
        f"(mix={args.mix}, seed={args.seed}) -> {args.out}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
