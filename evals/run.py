#!/usr/bin/env python3
"""Runs Nexus's model evaluation. See evals/README.md.

  python3 -I evals/run.py check                 offline: golden set shape, leakage, recorded verdicts
  python3 -I evals/run.py paid --i-accept-cost  golden cases through the pinned models (costs money)
        [--surface voice|mcp|all] [--runs 3] [--cases id,id] [--budget 3.0] [--label NAME]
  python3 -I evals/run.py report DIR            the table for a finished run, offline
  python3 -I evals/run.py calibrate --i-accept-cost   the honesty judge against held-out hand labels
"""

import argparse
import asyncio
import datetime as dt
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import nexus_evals as ne  # noqa: E402


def check(_args):
    cases = ne.load_cases()
    problems = ne.check_cases(cases)
    problems += [f"{cid}: golden input appears in a prompt or tool description: {text!r}" for cid, text in ne.leaks(cases)]
    for problem in problems:
        print("FAIL", problem)
    recorded = sorted((ne.ROOT / "recorded").glob("*.json"))
    agree = disagree = 0
    by_case = {c["id"]: c for c in cases}
    for path in recorded:
        rec = json.loads(path.read_text())
        for tr in rec["transcripts"]:
            verdicts = ne.evaluate(by_case[tr["case"]], tr)
            for ev, label in tr.get("labels", {}).items():
                if ev not in verdicts:
                    continue
                if verdicts[ev][0] == label:
                    agree += 1
                else:
                    disagree += 1
                    print(f"DISAGREE {path.name} {tr['case']} run {tr['run']} {ev}: evaluator {verdicts[ev][0]}, "
                          f"label {label} ({verdicts[ev][1]})")
    print(f"{len(cases)} cases, {len(problems)} problems; recorded verdicts vs hand labels: "
          f"{agree} agree, {disagree} disagree")
    return 1 if problems else 0


def paid(args):
    if not args.i_accept_cost:
        print("paid runs cost money: pass --i-accept-cost")
        return 2
    budget = ne.Budget(args.budget)
    cases = [c for c in ne.load_cases() if args.surface in ("all", c["surface"])]
    if args.cases:
        wanted = set(args.cases.split(","))
        cases = [c for c in cases if c["id"] in wanted]
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = ne.RUNS / f"{stamp}-{args.label}"
    out.mkdir(parents=True)
    judge = ne.make_judge(budget)
    results, transcripts = [], []
    start = budget.spent
    for case in cases:
        for run in range(args.runs):
            try:
                if case["surface"] == "voice":
                    tr = asyncio.run(ne.run_voice(case, budget, run))
                else:
                    tr = ne.run_mcp(case, budget, run)
            except Exception as e:  # an invalid run is counted and kept, never dropped silently
                results.append({"case": case["id"], "run": run, "error": f"{type(e).__name__}: {e}"})
                print(f"INVALID {case['id']} run {run}: {e}")
                if "budget" in str(e):
                    break
                continue
            verdicts = ne.evaluate(case, tr, judge)
            tr["verdicts"] = verdicts
            transcripts.append(tr)
            results.append({"case": case["id"], "run": run, "verdicts": verdicts})
            print(case["id"], run, {k: v[0] for k, v in verdicts.items()}, f"spent {budget.spent:.4f} USD")
    (out / "transcripts.json").write_text(json.dumps(transcripts, indent=1, ensure_ascii=False))
    (out / "results.json").write_text(json.dumps(results, indent=1))
    summary = ne.table(results, ne.load_cases())
    summary += f"\ncost of this run {budget.spent - start:.4f} USD, total logged {budget.spent:.4f} USD"
    (out / "summary.txt").write_text(summary + "\n")
    print(summary)
    print("written to", out)
    return 0


def report(args):
    d = pathlib.Path(args.dir)
    print(ne.table(json.loads((d / "results.json").read_text()), ne.load_cases()))
    return 0


def calibrate(args):
    """The judge's true-positive and true-negative rates on held-out hand labels. The dev split is for
    reading the judge's mistakes while its prompt is written; only the test split is reported."""
    if not args.i_accept_cost:
        print("calibration costs money: pass --i-accept-cost")
        return 2
    budget = ne.Budget(args.budget)
    judge = ne.make_judge(budget)
    labels = json.loads((ne.ROOT / "labels" / "honesty.json").read_text())
    for split in ("dev", "test"):
        pairs, rows = [], []
        for item in labels[split]:
            verdict, reason = judge(item["transcript"])
            if verdict is None:
                rows.append(f"  unparsable {item['id']}")
                continue
            pairs.append((item["label"], verdict))
            if verdict != item["label"]:
                rows.append(f"  miss {item['id']}: human {item['label']}, judge {verdict}: {reason}")
        q = ne.judge_quality(pairs)
        print(f"{split}: n={len(pairs)} TPR {q['tpr']:.2f} (n={q['n_pos']}) TNR {q['tnr']:.2f} (n={q['n_neg']})")
        print("\n".join(rows))
        if split == "test":
            (ne.ROOT / "labels" / "judge-quality.json").write_text(json.dumps(
                {"model": ne.PINNED["judge"], "pairs": pairs, **q}, indent=1) + "\n")
    print(f"total logged {budget.spent:.4f} USD")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check").set_defaults(fn=check)
    q = sub.add_parser("paid")
    q.add_argument("--i-accept-cost", action="store_true")
    q.add_argument("--surface", default="all", choices=["voice", "mcp", "all"])
    q.add_argument("--runs", type=int, default=3)
    q.add_argument("--cases")
    q.add_argument("--budget", type=float, default=3.0)
    q.add_argument("--label", default="run")
    q.set_defaults(fn=paid)
    r = sub.add_parser("report")
    r.add_argument("dir")
    r.set_defaults(fn=report)
    c = sub.add_parser("calibrate")
    c.add_argument("--i-accept-cost", action="store_true")
    c.add_argument("--budget", type=float, default=3.0)
    c.set_defaults(fn=calibrate)
    args = p.parse_args()
    sys.exit(args.fn(args))


if __name__ == "__main__":
    main()
