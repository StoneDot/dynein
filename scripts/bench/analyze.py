#!/usr/bin/env python3
"""Analyze a collected benchmark results tree (benchmark-plan.md §4/§5.3).

Walks the tree produced by collect.sh:

    <results>/<instance-type>/<cell-id>/rep<k>/{result.json,stats.jsonl,...}

parses result.json plus the per-second stats.jsonl emitted by the dy binary
(one JSON object per line: t, consumed_wcu, requests, throttled,
effective_target, resolved_items, failed_items, optional tokio), and prints a
markdown comparison table with mean +/- sigma across repetitions per
(instance type, cell, executor).

Metrics per run:
  wall_s      wall-clock seconds (/usr/bin/time via result.json)
  items_per_s final resolved_items / wall seconds
  tail_s      completion tail t(100%) - t(90%), from the resolved_items series
  adherence   mean +/- sigma of (per-interval consumed rate / effective
              target) over the saturated period
  waste_wcu   token waste = integral of max(0, target - consumed rate) over
              the saturated period ("saturated" = while resolved_items has
              not yet reached its final value, i.e. backlog exists)
  throttled   final cumulative throttled request count
  cpu_s       user + sys CPU seconds
  rss_mb      max RSS in MB

Missing or short stats series degrade to "n/a" instead of failing.
"""

import argparse
import json
import math
import os
import sys


def load_stats(path):
    """Parse a stats.jsonl file into a list of dicts sorted by t.

    Tolerates missing files, blank/truncated lines and missing keys.
    """
    if not os.path.isfile(path):
        return []
    points = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue  # truncated last line etc.
            if isinstance(obj, dict) and isinstance(obj.get("t"), (int, float)):
                points.append(obj)
    points.sort(key=lambda p: p["t"])
    return points


def series_metrics(points, wall_s):
    """Derive tail/adherence/waste/throttled from the per-second series."""
    out = {
        "items_per_s": None,
        "tail_s": None,
        "adherence": None,
        "waste_wcu": None,
        "throttled": None,
    }
    if not points:
        return out

    last = points[-1]
    final_resolved = last.get("resolved_items")
    if isinstance(last.get("throttled"), (int, float)):
        out["throttled"] = last["throttled"]

    if isinstance(final_resolved, (int, float)) and final_resolved > 0:
        duration = wall_s if wall_s else last["t"]
        if duration and duration > 0:
            out["items_per_s"] = final_resolved / duration

        t90 = t100 = None
        for p in points:
            r = p.get("resolved_items")
            if not isinstance(r, (int, float)):
                continue
            if t90 is None and r >= 0.9 * final_resolved:
                t90 = p["t"]
            if t100 is None and r >= final_resolved:
                t100 = p["t"]
                break
        if t90 is not None and t100 is not None:
            out["tail_s"] = t100 - t90
    else:
        t100 = None

    # Interval-based metrics over the saturated period (t <= t(100%)).
    saturated_end = t100 if t100 is not None else (points[-1]["t"] if points else None)
    ratios = []
    waste = 0.0
    have_waste = False
    for prev, cur in zip(points, points[1:]):
        needed = ("consumed_wcu", "effective_target")
        if not all(isinstance(cur.get(k), (int, float)) for k in needed):
            continue
        if not isinstance(prev.get("consumed_wcu"), (int, float)):
            continue
        dt = cur["t"] - prev["t"]
        if dt <= 0:
            continue
        if saturated_end is not None and cur["t"] > saturated_end:
            break
        rate = (cur["consumed_wcu"] - prev["consumed_wcu"]) / dt
        target = cur["effective_target"]
        if target > 0:
            ratios.append(rate / target)
        waste += max(0.0, target * dt - (cur["consumed_wcu"] - prev["consumed_wcu"]))
        have_waste = True
    if ratios:
        out["adherence"] = (mean(ratios), stdev(ratios))
    if have_waste:
        out["waste_wcu"] = waste
    return out


def mean(xs):
    return sum(xs) / len(xs)


def stdev(xs):
    if len(xs) < 2:
        return 0.0
    m = mean(xs)
    return math.sqrt(sum((x - m) ** 2 for x in xs) / (len(xs) - 1))


def collect_runs(root):
    """Yield (instance_type, run_dict) for every result.json under root."""
    runs = []
    for dirpath, _dirnames, filenames in os.walk(root):
        if "result.json" not in filenames:
            continue
        try:
            with open(os.path.join(dirpath, "result.json")) as f:
                result = json.load(f)
        except (OSError, json.JSONDecodeError) as e:
            print(f"warning: skipping {dirpath}: {e}", file=sys.stderr)
            continue
        # instance type: prefer result.json metadata, fall back to the path
        # component directly under root.
        rel = os.path.relpath(dirpath, root)
        instance = result.get("instance_type") or rel.split(os.sep)[0]
        stats = load_stats(os.path.join(dirpath, "stats.jsonl"))
        wall = result.get("wall_seconds")
        m = series_metrics(stats, wall)
        # Fallback items/s from result.json when the stats series is absent.
        if m["items_per_s"] is None and wall and result.get("items") \
                and result.get("exit_code") == 0:
            m["items_per_s"] = result["items"] / wall
        cpu = None
        if result.get("user_cpu_secs") is not None \
                and result.get("sys_cpu_secs") is not None:
            cpu = result["user_cpu_secs"] + result["sys_cpu_secs"]
        rss_mb = None
        if result.get("max_rss_kb") is not None:
            rss_mb = result["max_rss_kb"] / 1024.0
        runs.append({
            "instance": instance,
            "cell_id": result.get("cell_id", os.path.basename(os.path.dirname(dirpath))),
            "executor": result.get("executor", "?"),
            "mix": result.get("mix", "?"),
            "wcu": result.get("wcu"),
            "rep": result.get("rep"),
            "exit_code": result.get("exit_code"),
            "wall_s": wall,
            "cpu_s": cpu,
            "rss_mb": rss_mb,
            **m,
        })
    return runs


def fmt_ms(values, digits=1):
    """Format a list of numbers as 'mean +/- sigma' (markdown-safe)."""
    vals = [v for v in values if v is not None]
    if not vals:
        return "n/a"
    m, s = mean(vals), stdev(vals)
    return f"{m:.{digits}f} ± {s:.{digits}f}"


def fmt_adherence(pairs):
    """Adherence arrives as per-run (mean, sigma); aggregate the means."""
    vals = [p for p in pairs if p is not None]
    if not vals:
        return "n/a"
    means = [p[0] for p in vals]
    return f"{mean(means):.3f} ± {stdev(means):.3f}"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("results_dir", help="tree downloaded by collect.sh")
    parser.add_argument("--out", help="write markdown here instead of stdout")
    args = parser.parse_args()

    runs = collect_runs(args.results_dir)
    if not runs:
        print(f"no result.json found under {args.results_dir}", file=sys.stderr)
        return 1

    groups = {}
    for r in runs:
        groups.setdefault((r["instance"], r["cell_id"], r["executor"]), []).append(r)

    lines = []
    lines.append("# Benchmark comparison")
    lines.append("")
    lines.append(f"{len(runs)} runs, {len(groups)} cell groups. "
                 "Values are mean ± σ across repetitions.")
    lines.append("")
    header = ("| instance | cell | executor | mix | wcu | n | ok "
              "| wall s | items/s | tail s | adherence | waste WCU "
              "| throttled | cpu s | rss MB |")
    lines.append(header)
    lines.append("|" + "---|" * 15)

    def sort_key(key):
        instance, cell_id, executor = key
        sample = groups[key][0]
        return (instance, str(sample["mix"]), sample["wcu"] or 0, executor, cell_id)

    for key in sorted(groups, key=sort_key):
        instance, cell_id, executor = key
        reps = groups[key]
        ok = sum(1 for r in reps if r["exit_code"] == 0)
        row = [
            instance,
            cell_id,
            executor,
            str(reps[0]["mix"]),
            str(reps[0]["wcu"]),
            str(len(reps)),
            f"{ok}/{len(reps)}",
            fmt_ms([r["wall_s"] for r in reps]),
            fmt_ms([r["items_per_s"] for r in reps]),
            fmt_ms([r["tail_s"] for r in reps]),
            fmt_adherence([r["adherence"] for r in reps]),
            fmt_ms([r["waste_wcu"] for r in reps]),
            fmt_ms([r["throttled"] for r in reps], digits=0),
            fmt_ms([r["cpu_s"] for r in reps]),
            fmt_ms([r["rss_mb"] for r in reps]),
        ]
        lines.append("| " + " | ".join(row) + " |")

    failures = [r for r in runs if r["exit_code"] not in (0, None)]
    if failures:
        lines.append("")
        lines.append("## Failed runs")
        lines.append("")
        for r in failures:
            lines.append(f"- {r['instance']}/{r['cell_id']}/rep{r['rep']}: "
                         f"exit={r['exit_code']} (124 = timeout/hang)")

    text = "\n".join(lines) + "\n"
    if args.out:
        with open(args.out, "w") as f:
            f.write(text)
        print(f"wrote {args.out}", file=sys.stderr)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
