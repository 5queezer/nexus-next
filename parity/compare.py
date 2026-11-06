#!/usr/bin/env python3
"""Compare two parity trace directories. The first is the baseline.

Normalizations, applied to both sides. This is the whole list:

  generated timestamps  An ISO 8601 UTC string inside its trace's run window (startedAt to
                        finishedAt, +/- 5 s) becomes "<ts>". All real data predates any run, and
                        scenario inputs use fixed dates in early 2026, so stored and echoed
                        timestamps stay exact.
  new IDs               A numeric ID above id-floors.json becomes "<new:Kind:N>", numbered by first
                        appearance in the trace, so references must still agree with each other.
                        Real IDs stay exact.
  event cursors         A base64url event cursor is decoded, then its fields follow the two rules above.
  uploaded filenames    "filename" holding a UUID on a document with a new ID becomes "<generated>".
  derived request hash  "requestHash" on a submission that references a new ID becomes "<derived>",
                        because the hash covers those IDs. 52-submission-hash-real-ids uses real
                        IDs only, so there the hash stays exact.

Per-call modes, set in the scenario and copied to the trace:

  compare: "isError"        Only the error flag: schema-validator wording and Prisma messages,
                            neither of which a different implementation can reproduce.
  compare: "isError-set"    Within a parallel group, only the multiset of error flags: the race
                            winner legitimately varies, how many calls win does not. Groups are sorted
                            by flags first, so comparing by position compares the multisets.
  compare: "batch-results"  Exact, except that a per-item error holding a Prisma message only has
                            to be some error on the other side.
  allowDiff: "<reason>"     Differences are listed as documented and do not fail the run.

Option --unordered KEY sorts arrays of records under KEY by id on both sides first. Old Nexus returned
`contacts` and document `applications` in Postgres heap order, which no later read or restore promises,
so a server that returns them by id only differs there in order. Without the flag, order counts.

Values are never printed unless --show-values is given, since traces hold personal data.
"""
import argparse
import base64
import binascii
import collections
import datetime as dt
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FLOORS = json.loads((HERE / "id-floors.json").read_text())
TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$")
UUID_FILE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.\w+$")
NAMED_IDS = {
    "applicationId": "Application", "applicationIds": "Application", "existingId": "Application",
    "documentId": "Document", "documentIds": "Document",
    "submissionId": "ApplicationSubmission", "contactId": "Contact", "eventId": "ApplicationEvent",
}
PRISMA_ERROR = "invocation in\n"
ERROR_FLAGS = ("isError", "rpcError", "httpStatus")


def error_flags(call):
    return {key: bool(call.get(key)) for key in ERROR_FLAGS}


def parse_ts(value):
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def shape_kind(record, tool):
    if "originalName" in record:
        return "Document"
    if "occurredAt" in record:
        return "ApplicationEvent"
    if "company" in record:
        return "Application"
    if "submittedAt" in record:
        return "ApplicationSubmission"
    if "experience" in record:
        return "CvProfile"
    for fragment, kind in (("contact", "Contact"), ("document", "Document"), ("submission", "ApplicationSubmission"),
                           ("event", "ApplicationEvent"), ("activity", "ApplicationEvent")):
        if fragment in tool:
            return kind
    return "Application"


class Normalizer:
    def __init__(self, trace, drop_keys):
        slack = dt.timedelta(seconds=5)
        self.start = parse_ts(trace["startedAt"]) - slack
        self.end = parse_ts(trace["finishedAt"]) + slack
        self.drop_keys = drop_keys
        self.seen = {}
        self.counts = collections.Counter()
        self.tool = ""
        self.number = True

    def mask_id(self, kind, value):
        text = str(value)
        if not text.isdigit() or int(text) <= FLOORS[kind]:
            return value
        if not self.number:
            return f"<new:{kind}>"
        key = (kind, text)
        if key not in self.seen:
            self.counts[kind] += 1
            self.seen[key] = f"<new:{kind}:{self.counts[kind]}>"
        return self.seen[key]

    def cursor(self, value):
        try:
            decoded = json.loads(base64.urlsafe_b64decode(value + "=" * (-len(value) % 4)))
        except (binascii.Error, ValueError):
            return value
        if not (isinstance(decoded, dict) and {"occurredAt", "id"} <= decoded.keys()):
            return value
        return {"cursor": self.value({**decoded, "id": self.mask_id("ApplicationEvent", decoded["id"])})}

    def record(self, record):
        out = {}
        for key, item in record.items():
            if key in self.drop_keys:
                continue
            if key == "id":
                out[key] = self.mask_id(shape_kind(record, self.tool), item)
            elif key in NAMED_IDS and isinstance(item, (str, int)):
                out[key] = self.mask_id(NAMED_IDS[key], item)
            elif key in NAMED_IDS and isinstance(item, list):
                out[key] = [self.mask_id(NAMED_IDS[key], entry) for entry in item]
            elif key == "nextCursor" and isinstance(item, str):
                out[key] = self.cursor(item)
            else:
                out[key] = self.value(item)
        if isinstance(out.get("filename"), str) and str(out.get("id", "")).startswith("<new") and UUID_FILE.match(out["filename"]):
            out["filename"] = "<generated>"
        references = [out.get("applicationId"), *(out.get("documentIds") or [])]
        if "requestHash" in out and any(str(ref).startswith("<new") for ref in references):
            out["requestHash"] = "<derived>"
        return out

    def value(self, value):
        if isinstance(value, dict):
            return self.record(value)
        if isinstance(value, list):
            return [self.value(item) for item in value]
        if isinstance(value, str) and TIMESTAMP.match(value) and self.start <= parse_ts(value) <= self.end:
            return "<ts>"
        return value

    def call(self, call, number=True):
        self.tool, self.number = call["tool"], number
        fields = ("isError", "contentCount", "json", "text", "structured", "rpcError", "httpStatus")
        return {key: self.value(call[key]) for key in fields if key in call}


def load(directory):
    return {path.name: json.loads(path.read_text()) for path in sorted(Path(directory).glob("*.json"))}


def sort_unordered(value, keys):
    if isinstance(value, dict):
        for key, item in value.items():
            if key in keys and isinstance(item, list) and all(isinstance(i, dict) and "id" in i for i in item):
                item.sort(key=lambda i: int(i["id"]) if str(i["id"]).isdigit() else 0)
            sort_unordered(item, keys)
    elif isinstance(value, list):
        for item in value:
            sort_unordered(item, keys)


def normalized_calls(trace, drop_keys):
    """Parallel groups are sorted first, so completion order cannot affect comparison or numbering."""
    normalizer = Normalizer(trace, drop_keys)
    calls, index = [], 0
    source = trace["calls"]
    while index < len(source):
        group = source[index].get("group")
        members = [source[index]]
        while group is not None and index + len(members) < len(source) and source[index + len(members)].get("group") == group:
            members.append(source[index + len(members)])
        if group is not None:
            members.sort(key=lambda call: (sorted(error_flags(call).items()), json.dumps(normalizer.call(call, number=False), sort_keys=True)))
        calls.extend((call, normalizer.call(call)) for call in members)
        index += len(members)
    return calls


def describe(value):
    if isinstance(value, str):
        return f"str[{len(value)}]"
    if isinstance(value, list):
        return f"list[{len(value)}]"
    if isinstance(value, dict):
        return f"object[{len(value)}]"
    return "null" if value is None else type(value).__name__


def diff(a, b, path, show, out):
    if isinstance(a, dict) and isinstance(b, dict):
        for key in sorted(a.keys() | b.keys()):
            sub = f"{path}.{key}"
            if key not in a:
                out.append(f"{sub}: only in second ({describe(b[key])})")
            elif key not in b:
                out.append(f"{sub}: only in first ({describe(a[key])})")
            else:
                diff(a[key], b[key], sub, show, out)
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            out.append(f"{path}: length {len(a)} vs {len(b)}")
        for index, (left, right) in enumerate(zip(a, b)):
            diff(left, right, f"{path}[{index}]", show, out)
    elif a != b or type(a) is not type(b):
        detail = f"{json.dumps(a, ensure_ascii=False)} vs {json.dumps(b, ensure_ascii=False)}" if show else f"{describe(a)} vs {describe(b)}"
        out.append(f"{path}: {detail}")


def compare_call(call, a, b, show):
    mode = call.get("compare")
    if mode in ("isError", "isError-set"):
        out = []
        diff(error_flags(a), error_flags(b), "$", show, out)
        return out
    if mode == "batch-results":
        for left, right in zip(a.get("json", {}).get("results", []), b.get("json", {}).get("results", [])):
            if PRISMA_ERROR in str(left.get("error", "")) and right.get("error"):
                right["error"] = left["error"]
    out = []
    diff(a, b, "$", show, out)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("baseline")
    parser.add_argument("candidate")
    parser.add_argument("--show-values", action="store_true", help="print differing values (personal data)")
    parser.add_argument("--max-diffs", type=int, default=5, help="paths printed per call")
    parser.add_argument("--unordered", action="append", default=[], help="sort record arrays under this key by id")
    parser.add_argument("--drop-key", action="append", default=[], help="ignore this JSON key everywhere; for documented removals")
    args = parser.parse_args()

    baseline, candidate = load(args.baseline), load(args.candidate)
    sort_unordered(baseline, set(args.unordered))
    sort_unordered(candidate, set(args.unordered))
    drop_keys = set(args.drop_key)
    stats = collections.defaultdict(collections.Counter)
    failures, documented = [], []

    for name in sorted(baseline.keys() | candidate.keys()):
        if name not in baseline or name not in candidate:
            failures.append(f"{name}: trace only in {'candidate' if name not in baseline else 'baseline'}")
            continue
        left, right = normalized_calls(baseline[name], drop_keys), normalized_calls(candidate[name], drop_keys)
        if len(left) != len(right):
            failures.append(f"{name}: {len(left)} calls vs {len(right)}")
        for index, ((call, a), (other, b)) in enumerate(zip(left, right)):
            row = stats[call["tool"]]
            row["calls"] += 1
            if other.get("expectFailures"):
                failures.append(f"{name} call {index} {call['tool']}: candidate failed expectation on {', '.join(other['expectFailures'])}")
            problems = [f"tool {call['tool']} vs {other['tool']}"] if call["tool"] != other["tool"] else compare_call(call, a, b, args.show_values)
            if call.get("compare"):
                row["relaxed"] += 1
            if not problems:
                row["pass"] += 1
            elif call.get("allowDiff"):
                row["documented"] += 1
                documented.append(f"{name} call {index} {call['tool']}: {call['allowDiff']}")
            else:
                row["fail"] += 1
                head = f"{name} call {index} {call['tool']}"
                failures.append(head + "".join(f"\n    {problem}" for problem in problems[:args.max_diffs]))
                if len(problems) > args.max_diffs:
                    failures[-1] += f"\n    ... {len(problems) - args.max_diffs} more"

    contract = json.loads((HERE.parent / "contract" / "mcp-tools.snapshot.json").read_text())
    missing = sorted({tool["name"] for tool in contract["tools"]} - stats.keys())

    print(f"{'tool':34} {'calls':>6} {'pass':>6} {'fail':>6} {'doc':>5} {'relaxed':>8}")
    for tool, row in sorted(stats.items()):
        print(f"{tool:34} {row['calls']:6} {row['pass']:6} {row['fail']:6} {row['documented']:5} {row['relaxed']:8}")
    total = sum((row for row in stats.values()), collections.Counter())
    print(f"{'TOTAL':34} {total['calls']:6} {total['pass']:6} {total['fail']:6} {total['documented']:5} {total['relaxed']:8}")
    if drop_keys:
        print(f"ignored keys: {', '.join(sorted(drop_keys))}")
    for line in documented:
        print(f"documented: {line}")
    for tool in missing:
        failures.append(f"contract tool never called: {tool}")
    for line in failures:
        print(f"FAIL {line}")
    print(f"{len(failures)} differences")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
