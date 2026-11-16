#!/usr/bin/env python3
"""Runs Nexus's model evaluation. See evals/README.md.

  python3 -I evals/run.py check                 offline: golden set shape, leakage, recorded verdicts
  python3 -I evals/run.py paid --i-accept-cost  golden cases through the pinned models (costs money)
        [--surface voice|mcp|all] [--runs 3] [--cases id,id] [--budget 3.0] [--label NAME]
  python3 -I evals/run.py report DIR            the table for a finished run, offline
  python3 -I evals/run.py rejudge DIR --i-accept-cost   a finished run's honesty verdicts again
  python3 -I evals/run.py rerun DIR [--i-accept-cost]    runs the simulator now answers otherwise
  python3 -I evals/run.py calibrate --i-accept-cost   the honesty judge against held-out agent labels
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
    print(f"{len(cases)} cases, {len(problems)} problems; recorded verdicts vs agent labels: "
          f"{agree} agree, {disagree} disagree")
    # A changed evaluator that contradicts a recorded label fails here, so it fails cargo test too. A
    # label found wrong is corrected in recorded/, with the reason.
    return 1 if problems or disagree else 0


def paid(args):
    if not args.i_accept_cost:
        print("paid runs cost money: pass --i-accept-cost")
        return 2
    if not ne.in_run_window(ne.run_now({})):
        print("outside the run window (10:00 to 23:00 Madrid time, Tuesday to Sunday): the cases' times would not hold")
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


def rejudge(args):
    """The honesty verdicts of a finished run again, with the current judge. Other verdicts are code
    and are recomputed for free."""
    if not args.i_accept_cost:
        print("the judge costs money: pass --i-accept-cost")
        return 2
    d = pathlib.Path(args.dir)
    by_case = {c["id"]: c for c in ne.load_cases()}
    judge = ne.make_judge(ne.Budget(args.budget))
    transcripts = json.loads((d / "transcripts.json").read_text())
    results = [r for r in json.loads((d / "results.json").read_text()) if r.get("error")]
    for tr in transcripts:
        tr["verdicts"] = ne.evaluate(by_case[tr["case"]], tr, judge)
        results.append({"case": tr["case"], "run": tr["run"], "verdicts": tr["verdicts"]})
    (d / "transcripts.json").write_text(json.dumps(transcripts, indent=1, ensure_ascii=False))
    (d / "results.json").write_text(json.dumps(results, indent=1))
    print(ne.table(results, ne.load_cases()))
    return 0


def rerun(args):
    """Runs again, in place, every run of a finished run whose calls the simulated Nexus would now
    answer otherwise (`nexus_evals.diverges`), and rewrites its table."""
    d = pathlib.Path(args.dir)
    by_case = {c["id"]: c for c in ne.load_cases()}
    transcripts = json.loads((d / "transcripts.json").read_text())
    stale = [i for i, tr in enumerate(transcripts)
             if args.surface in ("all", tr.get("surface", by_case[tr["case"]]["surface"]))
             and ne.diverges(by_case[tr["case"]], tr)]
    print(f"{len(stale)} of {len(transcripts)} runs diverge:", sorted({transcripts[i]["case"] for i in stale}))
    if not stale or not args.i_accept_cost:
        return 0 if not stale else 2
    if not ne.in_run_window(ne.run_now({})):
        print("outside the run window (10:00 to 23:00 Madrid time, Tuesday to Sunday): the cases' times would not hold")
        return 2
    budget = ne.Budget(args.budget)
    judge = ne.make_judge(budget)
    for i in stale:
        old = transcripts[i]
        case = by_case[old["case"]]
        tr = asyncio.run(ne.run_voice(case, budget, old["run"])) if case["surface"] == "voice" \
            else ne.run_mcp(case, budget, old["run"])
        tr["verdicts"] = ne.evaluate(case, tr, judge)
        tr["rerun"] = True
        transcripts[i] = tr
        print(case["id"], old["run"], {k: v[0] for k, v in tr["verdicts"].items()}, f"spent {budget.spent:.4f} USD")
    invalid = [r for r in json.loads((d / "results.json").read_text()) if r.get("error")]
    results = invalid + [{"case": tr["case"], "run": tr["run"], "verdicts": tr["verdicts"]} for tr in transcripts]
    (d / "transcripts.json").write_text(json.dumps(transcripts, indent=1, ensure_ascii=False))
    (d / "results.json").write_text(json.dumps(results, indent=1))
    print(ne.table(results, ne.load_cases()))
    return 0


def report(args):
    if len(args.dir) > 1:
        # Several runs of one setup, read together: each case's runs from all of them.
        results = [r for d in args.dir for r in json.loads((pathlib.Path(d) / "results.json").read_text())]
        print(ne.table(results, ne.load_cases()))
        print(setups(args.dir))
        return 0
    d = pathlib.Path(args.dir[0])
    results = json.loads((d / "results.json").read_text())
    if args.reeval:
        # The code verdicts again, with the evaluators as they are now; the judge's stay as they were.
        by_case = {c["id"]: c for c in ne.load_cases()}
        transcripts = json.loads((d / "transcripts.json").read_text())
        for tr in transcripts:
            honest = tr["verdicts"].get("honest")
            tr["verdicts"] = ne.evaluate(by_case[tr["case"]], tr)
            if honest is not None:
                tr["verdicts"]["honest"] = honest
        results = [r for r in results if r.get("error")] + [
            {"case": tr["case"], "run": tr["run"], "verdicts": tr["verdicts"]} for tr in transcripts]
        (d / "transcripts.json").write_text(json.dumps(transcripts, indent=1, ensure_ascii=False))
        (d / "results.json").write_text(json.dumps(results, indent=1))
    print(ne.table(results, ne.load_cases()))
    print(setups(args.dir))
    return 0


def setups(dirs):
    """Which prompt and tools each run sent, by hash, so a table can be tied to what it measured."""
    seen = {}
    for d in dirs:
        for tr in json.loads((pathlib.Path(d) / "transcripts.json").read_text()):
            s = tr.get("setup")
            key = f"{tr.get('surface')} {s['model']} prompt {s['prompt']} tools {s['tools']}" if s else f"{tr.get('surface')} unrecorded"
            seen[key] = seen.get(key, 0) + 1
    return "setups: " + "; ".join(f"{k} ({n} runs)" for k, n in sorted(seen.items()))


def calibrate(args):
    """The judge's true-positive and true-negative rates on held-out agent labels (see labels/honesty.json for who wrote them). The dev split is for
    reading the judge's mistakes while its prompt is written; only the test split is reported."""
    if not args.i_accept_cost:
        print("calibration costs money: pass --i-accept-cost")
        return 2
    budget = ne.Budget(args.budget)
    judge = ne.make_judge(budget)
    labels = json.loads((ne.ROOT / "labels" / "honesty.json").read_text())
    for split in ("dev", "test"):
        pairs, rows, unparsable, verdicts = [], [], 0, {}
        for item in labels[split]:
            verdict, reason = judge(item["transcript"])
            verdicts[item["id"]] = verdict
            if verdict is None:
                unparsable += 1
                continue
            pairs.append((item["label"], verdict))
            if verdict != item["label"]:
                rows.append(f"  miss {item['id']}: label {item['label']}, judge {verdict}: {reason}")
        q = ne.judge_quality(pairs)
        print(f"{split}: n={len(pairs)} TPR {q['tpr']:.2f} (n={q['n_pos']}) TNR {q['tnr']:.2f} (n={q['n_neg']}), "
              f"unparsable {unparsable}")
        # The test split's mistakes stay unread, so that the judge's prompt is never fitted to them.
        if split == "dev":
            print("\n".join(rows))
        else:
            # Sorted, so that no position joins back to a test item: the misses are not to be read.
            (ne.ROOT / "labels" / "judge-quality.json").write_text(json.dumps(
                {"model": ne.PINNED["judge"], "test_set": labels.get("test_set", ""), "labels": "agent",
                 "unparsable": unparsable, "pairs": sorted(pairs), **q}, indent=1) + "\n")
            # Per item, the judge's verdicts would name its test misses, so they stay out of git.
            ne.RUNS.mkdir(exist_ok=True)
            (ne.RUNS / "judge-verdicts.json").write_text(json.dumps(verdicts, indent=1) + "\n")
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
    j = sub.add_parser("rejudge")
    j.add_argument("dir")
    j.add_argument("--i-accept-cost", action="store_true")
    j.add_argument("--budget", type=float, default=3.0)
    j.set_defaults(fn=rejudge)
    rr = sub.add_parser("rerun")
    rr.add_argument("dir")
    rr.add_argument("--i-accept-cost", action="store_true")
    rr.add_argument("--budget", type=float, default=3.0)
    rr.add_argument("--surface", default="all", choices=["voice", "mcp", "all"])
    rr.set_defaults(fn=rerun)
    r = sub.add_parser("report")
    r.add_argument("dir", nargs="+")
    r.add_argument("--reeval", action="store_true")
    r.set_defaults(fn=report)
    c = sub.add_parser("calibrate")
    c.add_argument("--i-accept-cost", action="store_true")
    c.add_argument("--budget", type=float, default=3.0)
    c.set_defaults(fn=calibrate)
    args = p.parse_args()
    sys.exit(args.fn(args))


if __name__ == "__main__":
    main()
