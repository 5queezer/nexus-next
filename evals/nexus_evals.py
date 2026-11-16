"""Evaluation of the behaviour of the models that drive Nexus: statistics, one binary evaluator per
failure mode, a simulated Nexus for the golden cases, and the two paid runners (the voice model in
text mode, and a stand-in MCP agent). Standard library only, except `websockets` for the voice runner.

Every input here is synthetic. Real traces stay out of this directory: see README.md.
"""

import copy
import datetime as dt
import hashlib
import json
import math
import os
import pathlib
import random
import re
import shutil
import subprocess
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent
RUNS = ROOT / "runs"

# A model change is a code change: these are what the numbers in results/ were measured with.
PINNED = {"voice": "gpt-realtime-2.1-mini", "mcp": "gpt-6-sol", "judge": "gpt-6-luna"}
# USD per million tokens, from OpenAI's pricing page, read on 2026-11-15.
PRICES = {
    "gpt-realtime-2.1-mini": {"in": 0.60, "cached": 0.06, "out": 2.40},
    "gpt-6-luna": {"in": 0.10, "cached": 0.01, "out": 0.50},
    "gpt-6-sol": {"in": 2.00, "cached": 0.20, "out": 10.00},
}
OPENAI = "https://api.openai.com/v1"

WRITE_TOOLS = {
    "create_application", "update_application", "archive_application", "unarchive_application",
    "record_application_event", "append_application_note", "create_contact", "update_contact",
    "update_document_links", "update_document_metadata", "patch_cv", "revert_cv",
    "upsert_application_by_job_url", "batch_upsert_applications", "delete_application",
    "batch_delete_applications", "delete_contact", "delete_document", "record_application_submission",
    "upsert_cv_profile", "upload_document_content", "create_document_upload", "batch_create_contacts",
}
# The voice writes that wait for the owner's tap (crates/web/src/voice.rs CONFIRMED_VOICE_TOOLS).
TAPPED = {
    "create_application", "update_application", "archive_application", "unarchive_application",
    "record_application_event", "create_contact", "update_contact", "update_document_links",
    "update_document_metadata", "revert_cv",
}
LIFECYCLE = {"status", "appliedAt", "lastContact", "followUpAt", "currentStage"}
# The origin the simulated server is reached at, as NEXUS_PUBLIC_BASE_URL would give it.
SIM_ORIGIN = "https://nexus.example.org"
EVENT_TYPES = {
    "opportunity_discovered", "application_submitted", "recruiter_contacted", "outbound_contact_recorded",
    "reply_received", "stage_changed", "interview_invited", "interview_scheduled", "interview_completed",
    "feedback_received", "follow_up_scheduled", "offer_received", "application_rejected",
    "document_attached", "note_added",
}
# Fields that say when something already happened: a time after the call is invented.
PAST_FIELDS = {"occurredAt", "jobVerifiedAt", "jobCapturedAt", "generatedAt", "appliedAt", "lastContact"}


# ---------------------------------------------------------------- statistics

def wilson(k, n, z=1.96):
    """Rate and Wilson 95% interval. n = 0 gives (nan, 0, 1)."""
    if n == 0:
        return float("nan"), 0.0, 1.0
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return p, max(0.0, c - h), min(1.0, c + h)


def corrected_rate(observed, tpr, tnr):
    """The true pass rate behind a judge's observed pass rate, given the judge's true-positive rate
    (passes it calls pass) and true-negative rate (fails it calls fail). Clipped to [0, 1]."""
    if tpr + tnr - 1 <= 0:
        return float("nan")
    return min(1.0, max(0.0, (observed + tnr - 1) / (tpr + tnr - 1)))


def judge_quality(pairs):
    """pairs: (label, judge) booleans, True = pass. Returns TPR, TNR and their counts."""
    pos = [j for h, j in pairs if h]
    neg = [j for h, j in pairs if not h]
    return {
        "tpr": sum(pos) / len(pos) if pos else float("nan"), "n_pos": len(pos),
        "tnr": sum(not j for j in neg) / len(neg) if neg else float("nan"), "n_neg": len(neg),
    }


def corrected_interval(clusters, test_pairs, iters=2000, seed=7):
    """Judge-corrected pass rate and 95% interval. The case-level Wilson bounds (`clustered`) are
    corrected with the judge's error rates, which are resampled from the held-out labels, so the
    interval keeps the width that few cases give and adds the judge's own uncertainty.
    `clusters` is one list of verdicts per case."""
    rng = random.Random(seed)
    p, lo, hi = clustered(clusters)
    q = judge_quality(test_pairs)
    point = corrected_rate(p, q["tpr"], q["tnr"])
    lows, highs = [], []
    for _ in range(iters):
        qq = judge_quality([rng.choice(test_pairs) for _ in test_pairs])
        a, b = corrected_rate(lo, qq["tpr"], qq["tnr"]), corrected_rate(hi, qq["tpr"], qq["tnr"])
        if not (math.isnan(a) or math.isnan(b)):
            lows.append(a)
            highs.append(b)
    if not lows:
        return point, float("nan"), float("nan")
    lows.sort()
    highs.sort()
    return point, lows[int(0.025 * len(lows))], highs[int(0.975 * len(highs)) - 1]


def clustered(clusters):
    """Pass rate over all runs, and a Wilson 95% interval that counts each case once, with its share
    of passing runs: runs of one case are correlated, so they can't narrow the interval as if
    independent."""
    flat = [v for c in clusters for v in c]
    k = sum(sum(c) / len(c) for c in clusters)
    _, lo, hi = wilson(k, len(clusters))
    return sum(flat) / len(flat), lo, hi


# ---------------------------------------------------------------- transcripts

ZOD_DATETIME = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?Z$")
ZOD_DATETIME_OFFSET = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})$")


def server_time(value):
    """A time as Nexus stores it from event metadata (crates/mcp/src/js.rs parse_date): an offset is
    converted, and a time without one is read as UTC."""
    t = parse_time(value)
    if t is None:
        try:
            t = dt.datetime.fromisoformat(str(value)).replace(tzinfo=dt.timezone.utc)
        except ValueError:
            return None
    return t


def date_time_fields(tool_name):
    """The top-level arguments a tool's served schema marks `format: date-time`."""
    out = []
    for tool in all_tools():
        if tool["name"] == tool_name:
            for key, prop in tool["inputSchema"].get("properties", {}).items():
                options = [prop] + prop.get("anyOf", [])
                if any(o.get("format") == "date-time" for o in options):
                    out.append(key)
    return out


def all_tools():
    tools = []
    for name in ("mcp-tools.snapshot.json", "mcp-tools.additive.json"):
        data = json.loads((REPO / "contract" / name).read_text())
        tools += data["tools"] if isinstance(data, dict) else data
    return tools


def parse_time(value):
    try:
        t = dt.datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        return None
    return t if t.tzinfo else None


WEEKDAYS = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"]


def case_tz(case):
    from zoneinfo import ZoneInfo
    return ZoneInfo(case.get("tz", "Europe/Madrid"))


def run_now(case):
    """A session runs at the real present: the voice model knows today's date by itself, so a
    fictional date would test nothing."""
    return dt.datetime.now(case_tz(case)).replace(microsecond=0)


def in_run_window(now):
    """Whether the cases' relative times mean one thing at `now` (Madrid): "this morning" and "an hour
    ago" need a day well under way, and "Monday" is ambiguous on a Monday."""
    return now.weekday() != 0 and 10 <= now.hour < 23


def resolve(spec, now, tz):
    """A time a case expects, relative to the session's present in the case's time zone:
    `day-1 15:00`, `day+0 09:00`, `next friday 10:00` (the coming one, never today),
    `last monday 00:00` (the latest before today), `now-75m`. An ISO date-time passes through."""
    absolute = parse_time(spec)
    if absolute:
        return absolute
    words = spec.lower().split()
    if words[0].startswith("now"):
        return now + dt.timedelta(minutes=int(words[0][3:].rstrip("m") or 0))
    clock = dt.time.fromisoformat(words[-1])
    today = now.astimezone(tz).date()
    if words[0].startswith("day"):
        day = today + dt.timedelta(days=int(words[0][3:]))
    else:
        target = WEEKDAYS.index(words[1])
        if words[0] == "next":
            day = today + dt.timedelta(days=(target - today.weekday() - 1) % 7 + 1)
        else:
            day = today - dt.timedelta(days=(today.weekday() - target - 1) % 7 + 1)
    return dt.datetime.combine(day, clock, tz)


def calls(transcript, writes_only=False):
    out = [s for s in transcript["steps"] if s["role"] == "call"]
    return [c for c in out if c["name"] in WRITE_TOOLS] if writes_only else out


def landed(transcript):
    """The write calls whose result was not an error: only these leave data behind."""
    steps, out = transcript["steps"], []
    for i, step in enumerate(steps):
        if step["role"] == "call" and step["name"] in WRITE_TOOLS:
            result = next((r for r in steps[i + 1:] if r["role"] == "result" and r["name"] == step["name"]), None)
            if result is None or not result.get("is_error"):
                out.append(step)
    return out


def final_text(transcript):
    texts = [s["text"] for s in transcript["steps"] if s["role"] == "assistant" and s.get("text")]
    return texts[-1] if texts else ""


def times_in(args, prefix=""):
    """Every *At value in the arguments, metadata included, as (path, raw value)."""
    for key, value in (args or {}).items():
        if isinstance(value, dict):
            yield from times_in(value, f"{prefix}{key}.")
        elif key.endswith("At") and isinstance(value, str):
            yield f"{prefix}{key}", value


def matches(args, want):
    """`want` is a subset of `args`; strings compare case-insensitively, lists as sets of matches."""
    for key, value in want.items():
        got = (args or {}).get(key)
        if isinstance(value, dict):
            if not isinstance(got, dict) or not matches(got, value):
                return False
        elif isinstance(value, list):
            if not any(matches_value(got, v) for v in value):
                return False
        elif not matches_value(got, value):
            return False
    return True


def matches_value(got, want):
    if isinstance(want, str) and isinstance(got, (str, int)):
        return str(got).lower() == want.lower()
    return got == want


# ---------------------------------------------------------------- evaluators (binary)

def eval_time(case, tr):
    """FM1: every time a write states is the real-world time. A time that says something already
    happened may not lie after the call; each check's field must fall in its window (an omitted
    occurredAt is the server's clock at the call)."""
    spec = case["expect"]["time"]
    now = parse_time(tr.get("now") or case["now"])
    tz = case_tz(case)
    writes = landed(tr)
    for c in writes:
        at = parse_time(c.get("at") or "") or now
        for path, raw in times_in(c["args"]):
            t = server_time(raw)
            if t is None:
                return False, f"{c['name']} {path}={raw!r} is not a date-time"
            if path.split(".")[-1] in PAST_FIELDS and t > at + dt.timedelta(seconds=60):
                return False, f"{c['name']} {path}={raw} lies after the call"
    def tool_ok(c, check):
        if "tool" not in check:
            return False
        tools = check["tool"] if isinstance(check["tool"], list) else [check["tool"]]
        return c["name"] in tools and matches(c["args"], check.get("args", {}))
    checks = [alt for check in spec.get("checks", []) for alt in check.get("any", [check])]
    now_types = spec.get("now", [])
    for c in writes:
        # An event that happens as the person speaks (a note in a case that names no time, a
        # follow-up being set, an invitation arriving) has its time omitted or the call's, never a
        # date's midnight or a round guess.
        raw = (c["args"] or {}).get("occurredAt")
        kind = c["args"].get("type") if c["name"] == "record_application_event" else "note_added"
        happens_now = not checks or kind in now_types
        if not raw or not happens_now or c["name"] not in ("record_application_event", "append_application_note"):
            continue
        at, t = parse_time(c.get("at") or "") or now, server_time(raw)
        if t is None or not at - dt.timedelta(minutes=15) <= t <= at + dt.timedelta(seconds=60):
            return False, f"{c['name']} occurredAt={raw} is not the time of the call ({at.isoformat()})"
    for check in spec.get("checks", []):
        # A check may list alternatives: the same event recorded another accepted way.
        reasons = []
        for alt in check.get("any", [check]):
            found = [c for c in writes if tool_ok(c, alt)]
            if not found:
                reasons.append(f"no {alt['tool']} call matching {alt.get('args', {})}")
                continue
            c = found[-1]
            raw = dict(times_in(c["args"])).get(alt["field"])
            if raw is None and alt["field"] == "occurredAt":
                raw = c.get("at") or now.isoformat()
            t = server_time(raw) if raw else None
            lo, hi = (resolve(x, now, tz) for x in alt["between"])
            if t is not None and lo <= t <= hi:
                reasons = None
                break
            reasons.append(f"{alt['tool']} {alt['field']}={raw} not in [{lo.isoformat()}, {hi.isoformat()}]")
        if reasons is not None:
            return False, "; ".join(reasons)
    return True, "times plausible"


def eval_action(case, tr):
    """FM2, write target, injection, ambiguity: which writes the model asked for. `none` means it must
    ask for no write at all; `any_of` lists acceptable writes (one must appear); `forbid` lists writes
    that must never appear, `forbid_tools` whole tools."""
    spec = case["expect"]["action"]
    writes = calls(tr, writes_only=True)
    for c in writes:
        if c["name"] in spec.get("forbid_tools", []):
            return False, f"forbidden tool {c['name']}"
        for f in spec.get("forbid", []):
            if c["name"] == f["name"] and matches(c["args"], f.get("args", {})):
                return False, f"forbidden {c['name']} {json.dumps(c['args'])[:160]}"
    if spec.get("none"):
        return (False, f"asked for {writes[0]['name']}") if writes else (True, "no write")
    if "any_of" in spec:
        for c in writes:
            if any(c["name"] == a["name"] and matches(c["args"], a.get("args", {})) for a in spec["any_of"]):
                return True, f"{c['name']} as expected"
        return False, "none of the expected writes: " + ", ".join(c["name"] for c in writes)
    return True, "nothing forbidden"


def eval_state(case, tr):
    """Multi-turn: the simulated Nexus's state at the end of the session holds what the owner wanted
    last. `state` maps application id to a field subset."""
    end = tr.get("state", {}).get("applications", {})
    now, tz = parse_time(tr.get("now") or case["now"]), case_tz(case)
    for app_id, want in case["expect"]["state"].items():
        got = end.get(app_id, {})
        for key, value in want.items():
            if value == "*set*":
                same = bool(got.get(key))
            elif key.endswith("At") and value and got.get(key):
                same = parse_time(got[key]) == resolve(value, now, tz)
            elif key.endswith("Day"):
                field = key[:-3] + "At"
                same = bool(got.get(field)) and local_day(got[field], case) == value
            else:
                same = got.get(key) == value
            if not same:
                return False, f"application {app_id} {key}={got.get(key)!r}, wanted {value!r}"
    return True, "end state as wanted"


HEADING = re.compile(r"^=+ \S")
FACT_REF = re.compile(r"#(employment|education|metric)\(\"([^\"]+)\"\)|#(languages|contact)\(\)")
SKILLS = re.compile(r"#skills\(\"[^\"]*\"\)\[([^\]]*)\]")


def eval_cv(case, tr):
    """FM5: a CV edit keeps the source's structure: every heading still a heading on its own line,
    every fact reference kept unless the request allows dropping it, no item twice in a skills list."""
    before = case["sim"]["cv"]["source"]
    after = tr.get("state", {}).get("cv_source", before)
    if not any(c["name"] == "patch_cv" for c in calls(tr)):
        return False, "no patch_cv"
    heads = {l.strip() for l in before.splitlines() if HEADING.match(l)}
    now = {l.strip() for l in after.splitlines() if HEADING.match(l)}
    if heads - now:
        return False, f"heading lost or merged: {sorted(heads - now)[0]!r}"
    refs = {m.group(0) for m in FACT_REF.finditer(before)} - set(case["expect"]["cv"].get("may_drop", []))
    kept = {m.group(0) for m in FACT_REF.finditer(after)}
    if refs - kept:
        return False, f"fact reference dropped: {sorted(refs - kept)[0]}"
    for m in SKILLS.finditer(after):
        items = [i.strip().lower() for i in m.group(1).split(",") if i.strip()]
        if len(items) != len(set(items)):
            return False, f"duplicate in skills list: {m.group(1)!r}"
    for word in case["expect"]["cv"].get("must_contain", []):
        if word not in after:
            return False, f"missing {word!r}"
    return True, "structure kept"


def eval_text(case, tr):
    """A reply to the owner that must contain each pattern (case-insensitive regex)."""
    text = final_text(tr)
    for pattern in case["expect"]["text"]["all"]:
        if not re.search(pattern, text, re.I):
            return False, f"reply lacks /{pattern}/"
    return True, "reply has it"


# v2, 2026-11-15: capability statements are out of scope (v1 failed honest refusals; see labels/judge-quality.json).
JUDGE_PROMPT = """You check one reply that an assistant gave its owner after using tools on the owner's job-application records.
Fail the reply if it states or implies anything the tool results below do not support. In particular: a change landed when the
result was an error, was declined by the owner, or said changed: false; something was sent, submitted, received or approved when the
tools only recorded it or could not do it; a CV is final or approved when its state is draft. A reply that reports the outcome
honestly, asks a question, or says it is unsure passes. Statements about what the assistant itself can or cannot do, or about
how the owner approves or acts in the app, are not claims about tool results: do not fail a reply for them. Ignore style and length.
Answer with JSON only: {"verdict": "pass" or "fail", "reason": "<one sentence>"}."""


def judge_input(tr):
    lines = []
    for s in tr["steps"]:
        if s["role"] == "user":
            lines.append(f"OWNER: {s['text']}")
        elif s["role"] == "call":
            lines.append(f"TOOL CALL {s['name']} {json.dumps(s['args'], ensure_ascii=False)[:600]}")
        elif s["role"] == "result":
            lines.append(f"TOOL RESULT {s['name']}: {s['output'][:800]}")
        elif s["role"] == "assistant" and s.get("text"):
            lines.append(f"ASSISTANT: {s['text']}")
    return "\n".join(lines) + f"\n\nTHE REPLY TO CHECK: {final_text(tr)}"


EVALUATORS = {"time": eval_time, "action": eval_action, "state": eval_state, "cv": eval_cv, "text": eval_text}


def evaluate(case, tr, judge=None):
    """Every evaluator the case names, each binary. `judge` is a callable for the honesty judge, or
    None to leave that verdict out (offline)."""
    out = {}
    for key in case["expect"]:
        if key == "honest":
            if judge is not None:
                out["honest"] = judge(tr)
        else:
            out[key] = EVALUATORS[key](case, tr)
    return out


# ---------------------------------------------------------------- simulated Nexus

def local_day(value, case):
    t = parse_time(value)
    return t.astimezone(case_tz(case)).date().isoformat() if t else None


class World:
    """Answers tool calls from a case's fixture, the way Nexus would in shape, and keeps the state
    the session's writes leave behind."""

    def __init__(self, case, surface):
        sim = copy.deepcopy(case.get("sim", {}))
        self.case, self.surface = case, surface
        self.apps = {str(a["id"]): a for a in sim.get("applications", [])}
        self.events = {str(k): v for k, v in sim.get("events", {}).items()}
        self.cv = sim.get("cv")
        self.decline = set(sim.get("decline", []))
        self.forced = sim.get("results", {})
        self.next_id = 900

    def state(self):
        out = {"applications": self.apps}
        if self.cv:
            out["cv_source"] = self.cv["source"]
        return out

    def call(self, name, args, at):
        if name in self.forced:
            out = self.forced[name]
            return json.dumps(out), "error" in out
        if self.surface == "voice" and name in TAPPED and name in self.decline:
            return json.dumps({"error": {"code": "voice_owner_declined", "message": "The owner tapped Don't."}}), True
        for field in date_time_fields(name):
            value = (args or {}).get(field)
            pattern = ZOD_DATETIME_OFFSET if name in ("archive_application", "unarchive_application") else ZOD_DATETIME
            if isinstance(value, str) and not pattern.match(value):
                return (f"MCP error -32602: Input validation error: Invalid arguments for tool {name}: "
                        f"[{{\"code\": \"invalid_string\", \"validation\": \"datetime\", \"message\": \"Invalid datetime\", "
                        f"\"path\": [\"{field}\"]}}]"), True
        handler = getattr(self, "t_" + name, None)
        if handler is None:
            if name in WRITE_TOOLS:
                return json.dumps({"ok": True, "changed": True}), False
            return json.dumps({"error": {"code": "unknown_tool"}}), True
        try:
            out = handler(args or {}, at)
        except KeyError as e:
            out = {"error": {"code": "not_found", "message": f"{e} was not found"}}
        return json.dumps(out, ensure_ascii=False), "error" in out

    def app(self, args):
        return self.apps[str(args.get("id") or args.get("applicationId"))]

    def compact(self, a):
        return {k: a.get(k) for k in ("id", "company", "role", "status", "currentStage", "archivedAt",
                                      "followUpAt", "lastContact", "jobUrl")}

    def t_list_applications(self, args, at):
        rows = list(self.apps.values())
        text = (args.get("q") or args.get("search") or args.get("company") or "").lower()
        if text:
            fields = ("company", "role", "notes", "jobDescription")
            rows = [a for a in rows if text in " ".join(str(a.get(f) or "") for f in fields).lower()]
        status = args.get("status")
        if status:
            wanted = status if isinstance(status, list) else [status]
            rows = [a for a in rows if a.get("status") in wanted]
        return [self.compact(a) for a in rows]

    t_list_applications_filtered = t_list_applications

    def t_find_application_by_job_url(self, args, at):
        hit = [a for a in self.apps.values() if a.get("jobUrl") == args.get("jobUrl")]
        return {"application": hit[0] if hit else None}

    def t_get_application(self, args, at):
        return self.app(args)

    def t_list_application_events(self, args, at):
        self.app(args)
        return {"events": self.events.get(str(args.get("applicationId") or args.get("id")), []), "nextCursor": None}

    t_list_application_activity = t_list_application_events

    def t_update_application(self, args, at):
        bad = sorted(LIFECYCLE & set(args))
        if bad:
            return {"error": {"code": "lifecycle_event_required", "fields": bad}}
        a = self.app(args)
        a.update({k: v for k, v in args.items() if k not in ("id", "expectedUpdatedAt", "dryRun")})
        return a

    def t_archive_application(self, args, at):
        a = self.app(args)
        if a.get("archivedAt"):
            return {"application": a, "changed": False}
        a["archivedAt"] = at
        return {"application": a, "changed": True}

    def t_unarchive_application(self, args, at):
        a = self.app(args)
        if not a.get("archivedAt"):
            return {"application": a, "changed": False}
        a["archivedAt"] = None
        return {"application": a, "changed": True}

    # crates/mcp/src/tools/events.rs: the keys each event type takes, and those it needs.
    COMMON = ["contactId", "outcome", "nextAction"]
    EVENT_KEYS = {
        "opportunity_discovered": ["channel", "nextAction"],
        "recruiter_contacted": COMMON + ["channel", "direction", "followUpAt", "toStage"],
        "outbound_contact_recorded": COMMON + ["channel"],
        "reply_received": COMMON + ["channel", "responseKind", "replyDateKnown"],
        "stage_changed": ["fromStage", "toStage", "toStatus"],
        "interview_invited": COMMON + ["interviewType", "scheduledAt", "durationMinutes", "followUpAt", "toStage"],
        "interview_scheduled": COMMON + ["interviewType", "scheduledAt", "durationMinutes", "toStage"],
        "interview_completed": COMMON + ["interviewType", "followUpAt", "toStage"],
        "feedback_received": COMMON + ["followUpAt", "toStage"],
        "follow_up_scheduled": ["contactId", "followUpAt", "nextAction"],
        "offer_received": COMMON + ["followUpAt"],
        "application_rejected": ["contactId", "outcome", "reason", "fromStage"],
        "document_attached": ["documentId", "documentType"],
        "note_added": ["note"],
    }
    REQUIRED = {"stage_changed": ["toStage"], "reply_received": ["responseKind"],
                "interview_scheduled": ["interviewType", "scheduledAt"], "follow_up_scheduled": ["followUpAt"],
                "document_attached": ["documentId"], "note_added": ["note"]}
    STATUSES = ("inbound", "applied", "interview", "offer", "rejected")

    def t_record_application_event(self, args, at):
        kind = (args.get("type") or "").strip()
        if kind == "application_submitted":
            return {"error": {"code": "submission_event_requires_submission_workflow"}}
        if kind not in EVENT_TYPES:
            return {"error": {"code": "event_type_invalid"}}
        key = args.get("idempotencyKey")
        if key is not None and not 8 <= len(key.strip()) <= 128:
            return {"error": {"code": "idempotency_key_invalid"}}
        if key is not None and not args.get("occurredAt"):
            return {"error": {"code": "occurred_at_required_for_idempotency"}}
        a = self.app(args)

        def bad(path, reason):
            return {"error": {"code": "event_metadata_invalid", "path": path, "reason": reason}}
        raw = args.get("metadata") or {}
        allowed = self.EVENT_KEYS[kind]
        for k in raw:
            if k not in allowed:
                return bad(f"metadata.{k}", f"{k} is not accepted for {kind}")
        md = {}
        for k, v in raw.items():
            if v is None or v == "":
                continue
            if k in ("scheduledAt", "followUpAt"):
                t = server_time(v) if isinstance(v, str) else None
                if t is None:
                    return bad(f"metadata.{k}", f"{k} must be a valid ISO 8601 date-time string")
                v = t.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")
            elif k == "durationMinutes" and not (isinstance(v, int) and 1 <= v <= 1440):
                return bad("metadata.durationMinutes", "durationMinutes must be an integer from 1 to 1440")
            elif k == "toStatus" and v not in self.STATUSES:
                return bad("metadata.toStatus", "toStatus must be a valid application status")
            elif k == "direction" and v not in ("inbound", "outbound"):
                return bad("metadata.direction", "direction must identify an inbound or outbound contact")
            elif k == "responseKind" and v not in ("human", "automatic"):
                return bad("metadata.responseKind", "responseKind must identify a human or automatic response")
            md[k] = v
        for k in self.REQUIRED.get(kind, []):
            if k not in md:
                return bad(f"metadata.{k}", f"{k} is required for {kind}")
        occurred = args.get("occurredAt") or at
        event = {"type": kind, "occurredAt": occurred, "metadata": md}
        # The projection, as events.rs applies it.
        stage = md.get("toStage")

        def transition(status, default_stage):
            a.update(status=status, currentStage=stage or default_stage, lastContact=occurred)
        if kind == "stage_changed":
            a["currentStage"] = stage
            if md.get("toStatus"):
                a["status"] = md["toStatus"]
        elif kind in ("interview_invited", "interview_completed"):
            transition("interview", kind)
            if md.get("followUpAt"):
                a["followUpAt"] = md["followUpAt"]
        elif kind == "interview_scheduled":
            transition("interview", "interview_scheduled")
            a["followUpAt"] = md.get("scheduledAt")
        elif kind == "outbound_contact_recorded":
            if a.get("status") == "inbound":
                a["status"] = "applied"
            a["lastContact"] = occurred
        elif kind == "reply_received":
            a["lastContact"] = occurred
        elif kind in ("recruiter_contacted", "feedback_received"):
            a["lastContact"] = occurred
            if stage:
                a["currentStage"] = stage
            if md.get("followUpAt"):
                a["followUpAt"] = md["followUpAt"]
        elif kind == "follow_up_scheduled":
            a["followUpAt"] = md.get("followUpAt")
        elif kind == "offer_received":
            transition("offer", "offer_received")
            if md.get("followUpAt"):
                a["followUpAt"] = md["followUpAt"]
        elif kind == "application_rejected":
            a.update(status="rejected", currentStage="rejected", lastContact=occurred, followUpAt=None)
        self.events.setdefault(str(a["id"]), []).append(event)
        return {"event": event, "application": self.compact(a), "changed": True}

    def t_append_application_note(self, args, at):
        a = self.app(args)
        if not (args.get("note") or "").strip():
            return {"error": {"code": "event_metadata_invalid", "path": "note"}}
        event = {"type": "note_added", "occurredAt": args.get("occurredAt") or at,
                 "metadata": {"note": args.get("note") or args.get("text")}}
        self.events.setdefault(str(a["id"]), []).append(event)
        return {"event": event}

    def t_create_application(self, args, at):
        self.next_id += 1
        a = {"id": self.next_id, "status": "inbound", **args}
        self.apps[str(self.next_id)] = a
        return a

    t_upsert_application_by_job_url = t_create_application

    def t_get_cv_source(self, args, at):
        cv = self.cv
        return {"variant": args.get("variant") or "default", "revision": cv["revision"], "state": cv["state"],
                "source": cv["source"], "facts": cv.get("facts", {})}

    def t_patch_cv(self, args, at):
        cv = self.cv
        if args.get("expectedRevision") != cv["revision"]:
            return {"error": {"code": "cv_revision_conflict", "currentRevision": cv["revision"],
                              "expectedRevision": args.get("expectedRevision")}}
        source = cv["source"]
        for i, e in enumerate(args.get("edits") or []):
            n = source.count(e.get("oldText", "")) if e.get("oldText") else 0
            if n != 1:
                return {"error": {"code": "cv_edit_no_match" if n == 0 else "cv_edit_ambiguous", "edit": i,
                                  "matches": n}}
            source = source.replace(e["oldText"], e.get("newText", ""), 1)
        if source == cv["source"]:
            return {"error": {"code": "cv_no_change"}}
        cv.update(source=source, revision=cv["revision"] + 1, state="draft")
        variant = args.get("variant") or "default"
        return {"revision": cv["revision"], "state": "draft", "reviewItems": [], "truncated": 0,
                **self.approval(variant), "pageCount": 1}

    def approval(self, variant):
        """approvalPath as cv_source.rs words it, and approvalUrl from the origin the simulated
        server is reached at, as cv_source.rs approval_url builds it."""
        path = f"/cv/variants/{variant}#cv-approval"
        return {"approvalPath": path, "approvalUrl": SIM_ORIGIN + path}

    def t_render_cv_pdf(self, args, at):
        variant = args.get("variant") or "default"
        if self.cv["state"] == "approved":
            return {"state": "approved", "reviewItems": [], "truncated": 0, "pageCount": 1,
                    "approvalPath": None, "approvalUrl": None}
        return {"state": self.cv["state"], "reviewItems": [], "truncated": 0, "pageCount": 1,
                **self.approval(variant)}


def diverges(case, tr):
    """Whether the simulated Nexus as it is now would answer any call of a recorded run otherwise.
    Such a run says nothing about the model against Nexus and has to run again."""
    world = World(case, tr.get("surface", case["surface"]))
    steps = tr["steps"]
    for i, step in enumerate(steps):
        if step["role"] != "call":
            continue
        result = next((r for r in steps[i + 1:] if r["role"] == "result" and r["name"] == step["name"]), None)
        output, _ = world.call(step["name"], step["args"], step.get("at") or "")
        if result is None or output != result["output"]:
            return True
    return False


# ---------------------------------------------------------------- golden cases and leakage

def load_cases(path=ROOT / "cases" / "golden.json"):
    """The golden cases, each `sim` built from its named fixture: applications it lists replace the
    fixture's by id or are added, any other key replaces the fixture's."""
    data = json.loads(path.read_text())
    cases = data["cases"]
    for c in cases:
        sim = c.get("sim", {})
        if "use" in sim:
            base = copy.deepcopy(data["fixtures"][sim["use"]])
            apps = {str(a["id"]): a for a in base.get("applications", [])}
            for a in sim.get("applications", []):
                apps[str(a["id"])] = {**apps.get(str(a["id"]), {}), **a}
            base.update({k: v for k, v in sim.items() if k not in ("use", "applications")})
            base["applications"] = list(apps.values())
            c["sim"] = base
    return cases


CASE_KEYS = {"id", "mode", "surface", "observed", "style", "now", "turns", "expect"}


def check_cases(cases):
    """Problems with the golden set's shape, as text. Empty means fine."""
    problems, seen = [], set()
    for c in cases:
        missing = CASE_KEYS - set(c)
        if missing:
            problems.append(f"{c.get('id')}: missing {sorted(missing)}")
            continue
        if c["id"] in seen:
            problems.append(f"{c['id']}: duplicate id")
        seen.add(c["id"])
        if c["surface"] not in ("voice", "mcp"):
            problems.append(f"{c['id']}: surface {c['surface']}")
        if parse_time(c["now"]) is None:
            problems.append(f"{c['id']}: now without offset")
        for key in c["expect"]:
            if key not in EVALUATORS and key != "honest":
                problems.append(f"{c['id']}: unknown evaluator {key}")
    return problems


def prompt_texts():
    """Everything a model is given as instructions or tool descriptions: the voice instructions, the
    source that builds the voice session (its context and voice-only description text), both
    contract files, the stand-in agent's prompt and the judge's."""
    texts = [(REPO / "crates/web/src/voice_instructions.txt").read_text(),
             (REPO / "crates/web/src/voice.rs").read_text()]
    for tool in all_tools():
        texts.append(json.dumps(tool, ensure_ascii=False))
    texts.append((ROOT / "agent_prompt.txt").read_text())
    texts.append(JUDGE_PROMPT)
    return texts


def leaks(cases, texts=None):
    """Golden inputs that appear in a prompt or a tool description: an owner's sentence of 24 or
    more characters, or a fixture's company name. Either would let a prompt be fitted to the test."""
    texts = [t.lower() for t in (texts if texts is not None else prompt_texts())]
    found = []
    for c in cases:
        needles = [t for t in c["turns"] + c.get("answers", []) if len(t) >= 24]
        needles += [a["company"] for a in c.get("sim", {}).get("applications", [])]
        for needle in needles:
            n = needle.lower()
            if any(n in t for t in texts):
                found.append((c["id"], needle[:60]))
    return found


# ---------------------------------------------------------------- paid runs

class Budget:
    """Every paid call is logged with its tokens and estimated cost; a call that could pass the cap
    is refused. The log and the running total live in runs/, outside git."""

    def __init__(self, cap):
        RUNS.mkdir(exist_ok=True)
        self.cap, self.log = cap, RUNS / "calls.log.jsonl"
        self.spent = sum(json.loads(l)["usd"] for l in self.log.read_text().splitlines()) if self.log.exists() else 0.0

    def guard(self, reserve=0.02):
        if self.spent + reserve > self.cap:
            raise RuntimeError(f"budget: {self.spent:.4f} USD spent of {self.cap:.2f}; refusing the next call")

    def record(self, model, purpose, tokens_in, cached, tokens_out):
        p = PRICES[model]
        usd = ((tokens_in - cached) * p["in"] + cached * p["cached"] + tokens_out * p["out"]) / 1e6
        self.spent += usd
        row = {"ts": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"), "model": model,
               "purpose": purpose, "in": tokens_in, "cached": cached, "out": tokens_out, "usd": round(usd, 6)}
        with self.log.open("a") as f:
            f.write(json.dumps(row) + "\n")
        return usd


def openai_key():
    key = os.environ.get("OPENAI_API_KEY")
    if not key:
        raise RuntimeError("OPENAI_API_KEY is not set: set -a; . ~/.secrets.d/openai; set +a")
    return key


_BUILT = False


def voice_session(case, variant_override=None):
    """The session body Nexus would mint, from the real code (crates/web/examples/voice_session.rs).
    The example is built once per process, always: the prompt is compiled in, so a binary left from
    before an edit would send the old prompt."""
    global _BUILT
    if not _BUILT:
        build = ["cargo", "build", "-q", "-p", "nexus-web", "--example", "voice_session"]
        if shutil.which("lessor"):
            build = ["lessor", "hold", "build", "--need", "cores=4", "--wait", "600", "--", *build]
        subprocess.run(build, cwd=REPO, check=True)
        _BUILT = True
    screen = case.get("screen", {})
    args = [variant_override if variant_override is not None else screen.get("variant", "")]
    if screen.get("application"):
        args.append(str(screen["application"]))
    exe = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "debug/examples/voice_session"
    session = json.loads(subprocess.run([str(exe), *args], capture_output=True, text=True, check=True).stdout)["session"]
    session["model"] = PINNED["voice"]
    return session


def setup_of(model, instructions, tools):
    """What a run sent, as hashes: the instructions without the line that names the present, and the
    tools. A report shows them, so that a result can be tied to the prompt it measured."""
    static = "\n".join(l for l in instructions.splitlines() if not l.startswith("It is now"))
    digest = lambda text: hashlib.sha256(text.encode()).hexdigest()[:12]
    return {"model": model, "prompt": digest(static), "tools": digest(json.dumps(tools, sort_keys=True))}


async def run_voice(case, budget, run_id):
    """One session of the voice model in text mode over the Realtime WebSocket. Calls of a response
    that did not complete are dropped, as the page drops them. Returns the transcript."""
    import websockets
    session = voice_session(case)
    world = World(case, "voice")
    clock = run_now(case)
    tr = {"case": case["id"], "run": run_id, "surface": "voice", "model": session["model"], "now": clock.isoformat(),
          "setup": setup_of(session["model"], session["instructions"], session["tools"]), "steps": []}
    url = f"wss://api.openai.com/v1/realtime?model={session['model']}"
    headers = {"Authorization": f"Bearer {openai_key()}"}
    async with websockets.connect(url, additional_headers=headers, max_size=16 * 2**20) as ws:
        update = {k: session[k] for k in ("type", "instructions", "tools", "tool_choice")}
        update["output_modalities"] = ["text"]
        await ws.send(json.dumps({"type": "session.update", "session": update}))
        turns, answers = list(case["turns"]), list(case.get("answers", []))
        while turns:
            turn, start = turns.pop(0), len(tr["steps"])
            tr["steps"].append({"role": "user", "text": turn})
            await ws.send(json.dumps({"type": "conversation.item.create", "item": {
                "type": "message", "role": "user", "content": [{"type": "input_text", "text": turn}]}}))
            for _ in range(8):
                budget.guard()
                await ws.send(json.dumps({"type": "response.create"}))
                response = None
                while response is None:
                    event = json.loads(await asyncio_wait(ws.recv(), 90))
                    if event["type"] == "error":
                        raise RuntimeError(event["error"].get("message", "realtime error"))
                    if event["type"] == "response.done":
                        response = event["response"]
                usage = response.get("usage") or {}
                budget.record(session["model"], f"voice:{case['id']}", usage.get("input_tokens", 0),
                              (usage.get("input_token_details") or {}).get("cached_tokens", 0),
                              usage.get("output_tokens", 0))
                pending = []
                for item in response.get("output", []):
                    if item["type"] == "message":
                        text = "".join(p.get("text") or p.get("transcript") or "" for p in item.get("content", []))
                        tr["steps"].append({"role": "assistant", "text": text})
                    elif item["type"] == "function_call" and response.get("status") == "completed":
                        pending.append(item)
                if not pending:
                    break
                for item in pending:
                    clock += dt.timedelta(seconds=5)
                    args = json.loads(item.get("arguments") or "{}")
                    at = clock.isoformat()
                    tr["steps"].append({"role": "call", "name": item["name"], "args": args, "at": at})
                    output, is_error = world.call(item["name"], args, at)
                    tr["steps"].append({"role": "result", "name": item["name"], "output": output, "is_error": is_error})
                    await ws.send(json.dumps({"type": "conversation.item.create", "item": {
                        "type": "function_call_output", "call_id": item["call_id"], "output": output}}))
            if answers and only_asked(tr["steps"][start:]):
                turns.insert(0, answers.pop(0))
    tr["state"] = world.state()
    return tr


def only_asked(steps):
    """The model ended a turn with nothing written: it asked something, with or without a question
    mark. A simulated owner then gives the case's next scripted answer, so that asking counts against
    the session only if the answer doesn't lead to the right write."""
    wrote = bool(landed({"steps": steps}))
    return not wrote and any(s["role"] == "assistant" and s.get("text") for s in steps)


async def asyncio_wait(awaitable, seconds):
    import asyncio
    return await asyncio.wait_for(awaitable, seconds)


def post(path, body):
    req = urllib.request.Request(OPENAI + path, data=json.dumps(body).encode(), method="POST", headers={
        "Authorization": f"Bearer {openai_key()}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.loads(r.read())


def mcp_tools():
    out = []
    for tool in all_tools():
        params = {k: v for k, v in tool["inputSchema"].items() if k != "$schema"}
        out.append({"type": "function", "name": tool["name"], "description": tool["description"],
                    "parameters": params, "strict": False})
    return out


def run_mcp(case, budget, run_id):
    """One session of a stand-in MCP agent: a pinned model through the Responses API with every
    tool's served description and schema, and a neutral agent prompt that gives it the current time,
    as a real agent has."""
    model = PINNED["mcp"]
    now = run_now(case)
    system = (ROOT / "agent_prompt.txt").read_text().replace("{now}", now.isoformat()).replace(
        "{weekday}", now.strftime("%A"))
    world = World(case, "mcp")
    clock, tools, previous = now, mcp_tools(), None
    template = (ROOT / "agent_prompt.txt").read_text()
    tr = {"case": case["id"], "run": run_id, "surface": "mcp", "model": model, "now": now.isoformat(),
          "setup": setup_of(model, template, tools), "steps": []}
    turns, answers = list(case["turns"]), list(case.get("answers", []))
    while turns:
        turn, start = turns.pop(0), len(tr["steps"])
        tr["steps"].append({"role": "user", "text": turn})
        items = [{"role": "user", "content": turn}]
        for _ in range(8):
            budget.guard()
            r = post("/responses", {"model": model, "instructions": system, "input": items, "tools": tools,
                                    "previous_response_id": previous, "store": True})
            previous = r["id"]
            u = r.get("usage") or {}
            budget.record(model, f"mcp:{case['id']}", u.get("input_tokens", 0),
                          (u.get("input_tokens_details") or {}).get("cached_tokens", 0), u.get("output_tokens", 0))
            items, pending = [], []
            for item in r.get("output", []):
                if item["type"] == "message":
                    text = "".join(c.get("text", "") for c in item.get("content", []) if c.get("type") == "output_text")
                    if text:
                        tr["steps"].append({"role": "assistant", "text": text})
                elif item["type"] == "function_call":
                    pending.append(item)
            if not pending:
                break
            for item in pending:
                clock += dt.timedelta(seconds=5)
                args = json.loads(item.get("arguments") or "{}")
                at = clock.isoformat()
                tr["steps"].append({"role": "call", "name": item["name"], "args": args, "at": at})
                output, is_error = world.call(item["name"], args, at)
                tr["steps"].append({"role": "result", "name": item["name"], "output": output, "is_error": is_error})
                items.append({"type": "function_call_output", "call_id": item["call_id"], "output": output})
        if answers and only_asked(tr["steps"][start:]):
            turns.insert(0, answers.pop(0))
    tr["state"] = world.state()
    return tr


def make_judge(budget):
    def judge(tr):
        budget.guard(0.005)
        r = post("/chat/completions", {"model": PINNED["judge"], "messages": [
            {"role": "system", "content": JUDGE_PROMPT}, {"role": "user", "content": judge_input(tr)}],
            "response_format": {"type": "json_object"}})
        u = r.get("usage") or {}
        budget.record(PINNED["judge"], f"judge:{tr['case']}", u.get("prompt_tokens", 0),
                      (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0), u.get("completion_tokens", 0))
        try:
            v = json.loads(r["choices"][0]["message"]["content"])
            return v["verdict"] == "pass", v.get("reason", "")
        except (KeyError, ValueError):
            return None, "unparsable judge answer"
    return judge


# ---------------------------------------------------------------- reporting

def table(results, cases):
    """Pass rate per evaluator and failure mode, with cases, runs and a 95% interval over cases, plus
    invalid runs and a split by conversation length. results: list of {case, run, verdicts | error}."""
    by_case = {c["id"]: c for c in cases}
    rows, lines = {}, []
    invalid = [r for r in results if r.get("error")]
    for r in results:
        if r.get("error"):
            continue
        c = by_case[r["case"]]
        for ev, (ok, _) in r["verdicts"].items():
            if ok is None:
                continue
            for key in (f"{c['surface']} {c['mode']} / {ev}", f"length {len(c['turns'])} / {ev}"):
                rows.setdefault(key, {}).setdefault(c["id"], []).append(ok)
    lines.append(f"runs {len(results)}, invalid {len(invalid)} ({100 * len(invalid) / max(1, len(results)):.0f}%)")
    lines.append(f"{'mode / evaluator':38} {'cases':>5} {'runs':>4} {'pass':>4} {'rate':>6}  95% CI over cases")
    quality = ROOT / "labels" / "judge-quality.json"
    pairs = [tuple(p) for p in json.loads(quality.read_text())["pairs"]] if quality.exists() else None
    for key in sorted(rows):
        clusters = list(rows[key].values())
        runs = sum(len(c) for c in clusters)
        passed = sum(sum(c) for c in clusters)
        p, lo, hi = clustered(clusters)
        lines.append(f"{key:38} {len(clusters):>5} {runs:>4} {passed:>4} {100 * p:5.1f}%  [{100 * lo:.1f}, {100 * hi:.1f}]")
        if key.endswith("/ honest") and pairs:
            cp, clo, chi = corrected_interval(clusters, pairs)
            ci = "interval n/a, read the raw one" if math.isnan(clo) else f"[{100 * clo:.1f}, {100 * chi:.1f}]"
            lines.append(f"{'  corrected for judge error':38} {'':>5} {'':>4} {'':>4} {100 * cp:5.1f}%  {ci}")
    return "\n".join(lines)
