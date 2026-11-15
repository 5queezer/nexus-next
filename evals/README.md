# Model evaluation

How well the models that drive Nexus behave: the voice assistant (gpt-realtime-2.1-mini with
`crates/web/src/voice_instructions.txt` and 23 tools) and the agents that use the MCP tools (Hermes,
Claude.ai, Claude Code), whose behaviour Nexus shapes through its tool descriptions, schemas, errors
and replies.

## Method

1. **Look at real behaviour before deciding what to measure.** The failure modes below come from
   reading real traces one by one (2026-11-15): 92 owner requests that called Nexus, from the Hermes
   career agent's sessions and Claude Code transcripts, plus the voice CV edits stored in production.
   Each trace got a free-form note on its first failing step; the notes were then grouped by root
   cause. The traces and notes are private and stay outside this repository.
2. **Specification first.** A failure the prompt or a tool description never ruled out is fixed
   there. Evaluators are for the failures that remain when the instruction is clear.
3. **One binary evaluator per failure mode**, in code wherever the answer is objective. The one
   judgement that needs a model, whether a reply claims more than the tools did, is checked by a
   judge that is only trusted after it is measured against held-out labels.
4. **Numbers come with n and a 95% interval**, decisions go by the interval's lower bound, every case
   runs several times, and invalid runs are counted and read. The runs of one case are not
   independent, so an interval counts each case once, with its share of passing runs.
5. **The golden set is a regression net**, not a quality claim: every case is synthetic and comes
   from an observed failure mode or an injection pattern. No golden input may appear in a prompt or a
   tool description; `run.py check` tests that.

## Failure modes measured

| Mode | Seen in real traces (first failure, n = 92) | Evaluator |
|---|---|---|
| FM1 event time is not when it happened (invented future times, "now" for a past event, relative dates) | 13, the largest mode; about 12% of the events agents wrote in production lie in the future | `time`: code |
| FM2 wrong event type or lifecycle meaning | 3 | `action`, `state`: code |
| FM3 a reply claims more than the tool results support | 2 first, 8 in all | `honest`: judge |
| FM5 a CV edit breaks the source's structure | 2, and one voice edit in production | `cv`: code |
| FM6 no usable approval link | 1 | `text`: code |
| FM12 a record reused for a different posting | 1 | `action`: code |
| Write target among same-company applications | 1 voice report | `action`: code |
| Prompt injection through job descriptions and stored notes, escalation | none seen; synthetic | `action`: code |

Cases also vary the speaker (terse, verbose, confrontational, changes mind), the language, and the
conversation length; a simulated owner answers a clarifying question with the case's scripted reply,
so success is judged per session.

## Running

Offline, part of `cargo test --workspace` (`crates/server/tests/evals_offline.rs`):

```sh
python3 -I evals/test_evals.py      # evaluators can pass and fail, statistics, simulated Nexus
python3 -I evals/run.py check       # golden set shape, leakage, recorded verdicts vs agent labels
```

Paid, only on purpose. It sends the golden cases (synthetic) to OpenAI, logs every call's tokens and
estimated cost to `evals/runs/calls.log.jsonl` and refuses a call that could pass `--budget`:

```sh
cargo build -p nexus-web --example voice_session     # the exact session a mint asks OpenAI for
set -a; . ~/.secrets.d/openai; set +a
python3 evals/run.py paid --i-accept-cost --runs 3 --label baseline
python3 evals/run.py report evals/runs/<run>
python3 evals/run.py calibrate --i-accept-cost        # the honesty judge against held-out labels
```

A session runs at the real present: the voice model knows today's date by itself, so expected times
are relative to the run (`day-1 15:00`, `next friday 10:00`). Run between 10:00 and 23:00 Madrid
time, Tuesday to Sunday.

Pinned models are in `nexus_evals.PINNED`: a model change is a code change, and the numbers in
`results/` hold only for the models they name. The MCP side uses a stand-in agent: gpt-6-sol, one of
the model families in the real Hermes sessions (gpt-6-astra 27, gpt-5.6-sol 15, claude-sonnet-5-5 14, gpt-6-sol
11 of the exported sessions), with every served tool description and
`agent_prompt.txt`. It measures what the descriptions make such a model do, not Hermes itself.

The simulated Nexus answers as the server does where a case can reach it: date rules from
`zod.rs` and `js.rs`, event types, keys, enums and the status projection from `events.rs`, the
lifecycle refusal, `changed: false` and a declined card. When it is corrected, `run.py rerun`
replays every recorded run and runs again those it would now answer otherwise.

The honesty judge's quality comes from a test set labelled after its prompt was last changed; its
mistakes on that set are never printed, so the prompt can't be fitted to them. Each label set says
how it was made.

## Files

- `cases/golden.json`: the golden cases and their fixture.
- `nexus_evals.py`: statistics, evaluators, simulated Nexus, runners.
- `recorded/`: transcripts of past paid runs with agent labels, which `check` compares with the
  evaluators' verdicts (false passes and false fails).
- `labels/`: agent labels for the honesty judge, split into dev and test.
- `results/`: summaries of runs that changes are measured against.
- `owner_review.py`: the owner's labelling pass. `build` writes one private page (no network, no
  library) with the judge's test items and a sample of Phase A traces with their agent notes; the
  owner labels Pass, Fail or Defer with a note, and exports a JSON file. `score` then reports the
  agreement between the owner's and the agent labels, and the judge's TPR and TNR against the
  owner's. The page holds private traces, so it is built outside this repository.

## Limits

- **Labels are agent labels, not human ground truth.** The labels in `labels/` and `recorded/` were
  written by the lead agent (Claude Opus 5.5) reading each transcript, not by a person. The
  honesty judge is another model family (gpt-6-luna), which lowers but does not remove the risk
  that labeller and judge share blind spots. Judge numbers are "against agent labels" until the
  owner's labelling pass (`owner_review.py`) gives human labels; then agreement between the two
  and the judge's TPR/TNR against the owner's labels are what counts.
- **Real dishonest replies are rare**, so the judge's true-negative rate is measured on replies
  with an overclaim added by the lead agent, marked `perturbed`.
- **The Phase A open coding was done by note-taking agents** under a fixed brief, grouped by the
  lead and spot-checked by the review agent; the owner's check of 20 random traces is pending.
- **The MCP side cannot show the real agents' timestamp failures.** gpt-6-sol given the current
  time in a short session passes every time case, while real Hermes sessions (long, compacted, run
  on a schedule) did not. A description change is measured there as "no harm" only.
- **Voice runs in text mode**: no audio and no speech recognition, so mishearing is only covered by
  cases written as a recogniser might render speech.
- **The simulated Nexus** mirrors the server where the cases reach it; `run.py rerun` keeps old
  runs honest when it changes. Other differences are possible.
- **Runs happen at the real present**, so a case's relative times depend on when it runs; see
  Running.
