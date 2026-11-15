#!/usr/bin/env python3
"""Offline checks of the evaluation code: each evaluator can pass and can fail, the statistics are
right on known values, the simulated Nexus answers as Nexus does, and the golden set is well formed
and absent from every prompt. Synthetic data only. Run: python3 -I evals/test_evals.py"""

import json
import pathlib
import re
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import nexus_evals as ne  # noqa: E402

CASES = {c["id"]: c for c in ne.load_cases()}


def tr(*steps, state=None):
    out = {"case": "t", "run": 0, "steps": list(steps)}
    if state is not None:
        out["state"] = state
    return out


def call(name, at="2026-03-04T09:15:05+00:00", **args):
    return {"role": "call", "name": name, "args": args, "at": at}


def say(text):
    return {"role": "assistant", "text": text}


class Statistics(unittest.TestCase):
    def test_wilson_matches_known_values(self):
        p, lo, hi = ne.wilson(27, 232)
        self.assertAlmostEqual(p, 0.1164, places=3)
        self.assertAlmostEqual(lo, 0.0813, places=3)
        self.assertAlmostEqual(hi, 0.1640, places=3)
        self.assertEqual(ne.wilson(0, 10)[1], 0.0)

    def test_corrected_rate_undoes_judge_error(self):
        # A judge with TPR 0.9 and TNR 0.8 sees 0.9*0.4 + 0.2*0.6 = 0.48 passes when 40% truly pass.
        self.assertAlmostEqual(ne.corrected_rate(0.48, 0.9, 0.8), 0.4, places=6)
        self.assertTrue(ne.corrected_rate(0.5, 0.5, 0.5) != ne.corrected_rate(0.5, 0.5, 0.5))  # nan: useless judge

    def test_corrected_interval_contains_the_point(self):
        runs = [True] * 30 + [False] * 20
        pairs = [(True, True)] * 18 + [(True, False)] * 2 + [(False, False)] * 17 + [(False, True)] * 3
        point, lo, hi = ne.corrected_ci(runs, pairs, iters=500)
        self.assertTrue(lo <= point <= hi)


class RelativeTimes(unittest.TestCase):
    def test_weekdays_resolve_around_today(self):
        tz = ne.case_tz({})
        wednesday = ne.parse_time("2026-03-04T10:15:00+01:00")
        self.assertEqual(ne.resolve("next friday 10:00", wednesday, tz).isoformat(), "2026-03-06T10:00:00+01:00")
        self.assertEqual(ne.resolve("next wednesday 10:00", wednesday, tz).date().isoformat(), "2026-03-11")
        self.assertEqual(ne.resolve("last monday 00:00", wednesday, tz).date().isoformat(), "2026-03-02")
        self.assertEqual(ne.resolve("last wednesday 00:00", wednesday, tz).date().isoformat(), "2026-02-25")
        self.assertEqual(ne.resolve("day-1 15:00", wednesday, tz).isoformat(), "2026-03-03T15:00:00+01:00")
        self.assertEqual(ne.resolve("now-60m", wednesday, tz).isoformat(), "2026-03-04T09:15:00+01:00")

    def test_summer_time_is_the_zones_own(self):
        tz = ne.case_tz({})
        july = ne.parse_time("2026-07-01T12:00:00+02:00")
        self.assertEqual(ne.resolve("day+1 09:00", july, tz).isoformat(), "2026-07-02T09:00:00+02:00")


class TimeEvaluator(unittest.TestCase):
    case = CASES["time-yesterday-interview"]

    def test_the_right_day_passes(self):
        t = tr(call("record_application_event", applicationId="15", type="interview_completed",
                    occurredAt="2026-03-03T15:00:00Z"))
        self.assertTrue(ne.eval_time(self.case, t)[0])

    def test_an_omitted_time_is_the_call_time_and_fails(self):
        t = tr(call("record_application_event", applicationId="15", type="interview_completed"))
        self.assertFalse(ne.eval_time(self.case, t)[0])

    def test_an_invented_future_time_fails_on_any_write(self):
        note = CASES["time-note-no-invented-time"]
        t = tr(call("append_application_note", applicationId="14", note="x", occurredAt="2026-03-04T09:30:00Z"))
        ok, why = ne.eval_time(note, t)
        self.assertFalse(ok)
        self.assertIn("after the call", why)
        t = tr(call("append_application_note", applicationId="14", note="x", occurredAt="2026-03-04T09:15:00Z"))
        self.assertTrue(ne.eval_time(note, t)[0])

    def test_a_midnight_guess_for_something_done_now_fails(self):
        note = CASES["time-note-no-invented-time"]
        t = tr(call("append_application_note", applicationId="14", note="x", occurredAt="2026-03-04T00:00:00Z"))
        self.assertFalse(ne.eval_time(note, t)[0])
        t = tr(call("append_application_note", applicationId="14", note="x", occurredAt="2026-03-04T09:10:00Z"))
        self.assertTrue(ne.eval_time(note, t)[0])

    def test_a_refused_write_leaves_nothing_to_judge(self):
        note = CASES["time-note-no-invented-time"]
        t = tr(call("append_application_note", applicationId="14", note="x", occurredAt="2026-03-04T12:00:00Z"),
               {"role": "result", "name": "append_application_note", "output": "{}", "is_error": True})
        self.assertTrue(ne.eval_time(note, t)[0])

    def test_a_metadata_time_without_offset_is_read_as_utc_like_the_server(self):
        case = CASES["time-followup-friday"]
        t = tr(call("record_application_event", applicationId="16", type="follow_up_scheduled",
                    metadata={"followUpAt": "2026-03-06T10:00:00"}))
        ok, why = ne.eval_time(case, t)
        self.assertFalse(ok, "10:00 read as UTC is 11:00 in Madrid")

    def test_metadata_times_are_checked_by_path(self):
        case = CASES["time-followup-friday"]
        good = tr(call("record_application_event", applicationId="16", type="follow_up_scheduled",
                       metadata={"followUpAt": "2026-03-06T10:00:00+01:00"}))
        bad = tr(call("record_application_event", applicationId="16", type="follow_up_scheduled",
                      metadata={"followUpAt": "2026-03-13T10:00:00+01:00"}))
        self.assertTrue(ne.eval_time(case, good)[0])
        self.assertFalse(ne.eval_time(case, bad)[0])


class ActionEvaluator(unittest.TestCase):
    def test_the_archived_duplicate_fails_and_the_active_one_passes(self):
        case = CASES["target-archived-duplicate"]
        self.assertFalse(ne.eval_action(case, tr(call("archive_application", id="14")))[0])
        self.assertTrue(ne.eval_action(case, tr(call("archive_application", id=31)))[0])
        self.assertFalse(ne.eval_action(case, tr(call("archive_application", id="14"), call("archive_application", id="31")))[0])

    def test_none_means_no_write_at_all(self):
        case = CASES["inject-job-description"]
        self.assertTrue(ne.eval_action(case, tr(call("get_application", id="20"), say("It needs Go.")))[0])
        self.assertFalse(ne.eval_action(case, tr(call("get_application", id="20"),
                                                 call("record_application_event", applicationId="20",
                                                      type="offer_received")))[0])

    def test_forbidden_event_type_fails_even_with_an_allowed_write(self):
        case = CASES["event-posting-closed"]
        t = tr(call("record_application_event", applicationId="20", type="application_rejected"),
               call("archive_application", id="20"))
        self.assertFalse(ne.eval_action(case, t)[0])
        self.assertTrue(ne.eval_action(case, tr(call("archive_application", id="20")))[0])

    def test_a_list_of_types_accepts_any_of_them(self):
        case = CASES["event-recruiter-intro-call"]
        self.assertTrue(ne.eval_action(case, tr(call("record_application_event", applicationId="21",
                                                     type="reply_received")))[0])
        self.assertFalse(ne.eval_action(case, tr(call("record_application_event", applicationId="21",
                                                      type="interview_completed")))[0])


class CvEvaluator(unittest.TestCase):
    case = CASES["cv-compress-one-page"]
    source = case["sim"]["cv"]["source"]

    def run_edit(self, old, new):
        world = ne.World(self.case, "voice")
        out, err = world.call("patch_cv", {"expectedRevision": 3, "summary": "s",
                                           "edits": [{"oldText": old, "newText": new}]}, "2026-03-04T09:15:00Z")
        self.assertFalse(err, out)
        return tr(call("patch_cv"), state=world.state())

    def test_a_heading_merged_into_the_paragraph_fails(self):
        # The real voice edit lost the line break before "= Skills".
        t = self.run_edit("review.\n\n= Skills", "review.= Skills")
        ok, why = ne.eval_cv(self.case, t)
        self.assertFalse(ok)
        self.assertIn("Skills", why)

    def test_a_duplicate_skill_fails(self):
        t = self.run_edit("[TypeScript, React, Vue.js]", "[TypeScript, React, React]")
        self.assertFalse(ne.eval_cv(self.case, t)[0])

    def test_a_dropped_job_fails(self):
        start = self.source.index('#employment("globex-dev")')
        end = self.source.index("= Education")
        t = self.run_edit(self.source[start:end], "")
        self.assertFalse(ne.eval_cv(self.case, t)[0])

    def test_a_shorter_summary_passes(self):
        t = self.run_edit("I build APIs and internal tools with Python, TypeScript and Rust. ", "")
        self.assertTrue(ne.eval_cv(self.case, t)[0])


class SimulatedValidation(unittest.TestCase):
    def test_top_level_times_need_z_except_on_archive(self):
        world = ne.World(CASES["time-note-no-invented-time"], "mcp")
        for value in ("2026-03-04T09:00:00", "2026-03-04T10:00:00+01:00"):
            out, err = world.call("append_application_note", {"applicationId": "14", "note": "n",
                                                              "occurredAt": value, "idempotencyKey": "k-12345678"}, "t")
            self.assertTrue(err, value)
            self.assertIn("Invalid datetime", out)
        out, err = world.call("append_application_note", {"applicationId": "14", "note": "n",
                                                          "occurredAt": "2026-03-04T09:00Z", "idempotencyKey": "k-12345678"}, "t")
        self.assertFalse(err, out)
        out, err = world.call("archive_application", {"id": "14", "archivedAt": "2026-03-04T10:00:00+01:00"}, "t")
        self.assertFalse(err, out)

    def test_metadata_times_are_stored_in_utc(self):
        world = ne.World(CASES["time-followup-friday"], "voice")
        out, _ = world.call("record_application_event", {"applicationId": "16", "type": "follow_up_scheduled",
                                                         "metadata": {"followUpAt": "2026-03-06T10:00:00+01:00"}}, "t")
        self.assertEqual(json.loads(out)["event"]["metadata"]["followUpAt"], "2026-03-06T09:00:00.000Z")


class StateAndText(unittest.TestCase):
    def test_change_of_mind_needs_the_first_write_undone(self):
        case = CASES["multi-change-mind"]
        world = ne.World(case, "voice")
        world.call("archive_application", {"id": "19"}, "2026-03-04T09:15:05Z")
        world.call("archive_application", {"id": "20"}, "2026-03-04T09:15:10Z")
        self.assertFalse(ne.eval_state(case, tr(state=world.state()))[0])
        world.call("unarchive_application", {"id": "19"}, "2026-03-04T09:15:15Z")
        self.assertTrue(ne.eval_state(case, tr(state=world.state()))[0])

    def test_follow_up_state_compares_instants(self):
        case = CASES["multi-contradiction"]
        world = ne.World(case, "voice")
        world.call("record_application_event", {"applicationId": "16", "type": "follow_up_scheduled",
                                                "metadata": {"followUpAt": "2026-03-09T10:00:00+01:00"}}, "x")
        self.assertTrue(ne.eval_state(case, tr(state=world.state()))[0])

    def test_approval_link_needs_an_absolute_url(self):
        case = CASES["mcp-approval-link"]
        self.assertFalse(ne.eval_text(case, tr(say("Approve it at /cv/variants/default?view=review")))[0])
        self.assertTrue(ne.eval_text(case, tr(say("Approve it at https://nexus.example.org/cv/variants/default?view=review")))[0])


class SimulatedNexus(unittest.TestCase):
    def test_lifecycle_fields_are_refused_on_update(self):
        world = ne.World(CASES["event-mark-interviewing"], "voice")
        out, err = world.call("update_application", {"id": "17", "status": "interview"}, "t")
        self.assertTrue(err)
        self.assertEqual(json.loads(out)["error"]["code"], "lifecycle_event_required")

    def test_archiving_twice_changes_nothing(self):
        world = ne.World(CASES["honest-already-archived"], "voice")
        out, _ = world.call("archive_application", {"id": "19"}, "t")
        self.assertIs(json.loads(out)["changed"], False)

    def test_a_declined_card_answers_like_the_page(self):
        world = ne.World(CASES["honest-declined"], "voice")
        out, err = world.call("archive_application", {"id": "19"}, "t")
        self.assertTrue(err)
        self.assertEqual(json.loads(out)["error"]["code"], "voice_owner_declined")
        self.assertIsNone(world.apps["19"]["archivedAt"])

    def test_a_note_without_time_takes_the_call_time(self):
        world = ne.World(CASES["time-note-no-invented-time"], "mcp")
        out, _ = world.call("append_application_note", {"applicationId": "14", "note": "n"}, "2026-03-04T09:15:05Z")
        self.assertEqual(json.loads(out)["event"]["occurredAt"], "2026-03-04T09:15:05Z")


class GoldenSet(unittest.TestCase):
    def test_cases_are_well_formed(self):
        self.assertEqual(ne.check_cases(list(CASES.values())), [])

    def test_no_golden_input_is_in_a_prompt(self):
        self.assertEqual(ne.leaks(list(CASES.values())), [])

    def test_the_leak_check_can_fail(self):
        case = CASES["time-yesterday-interview"]
        planted = ["Example: " + case["turns"][0]]
        self.assertTrue(ne.leaks([case], planted))
        self.assertTrue(ne.leaks([CASES["target-archived-duplicate"]], ["... Example Robotics ..."]))

    def test_every_failure_mode_has_cases_on_both_surfaces_where_planned(self):
        modes = {(c["mode"], c["surface"]) for c in CASES.values()}
        for needed in [("FM1", "voice"), ("FM1", "mcp"), ("FM2", "voice"), ("FM2", "mcp"), ("FM3", "voice"),
                       ("FM5", "voice"), ("FM6", "mcp"), ("target", "voice"), ("injection", "voice")]:
            self.assertIn(needed, modes)

    def test_the_pinned_voice_model_is_the_servers_default(self):
        config = (ne.REPO / "crates/server/src/config.rs").read_text()
        default = re.search(r'NEXUS_REALTIME_MODEL"?,?[^)]*default_value = "([^"]+)"', config, re.S)
        default = default.group(1) if default else re.search(r'default_value = "(gpt-realtime[^"]+)"', config).group(1)
        self.assertEqual(ne.PINNED["voice"], default)

    def test_every_pinned_model_has_a_price(self):
        for model in ne.PINNED.values():
            self.assertIn(model, ne.PRICES)


if __name__ == "__main__":
    unittest.main()
