#!/usr/bin/env python3
"""Checks that compare.py can fail. Synthetic traces only. Run: python3 parity/test_compare.py"""
import json
import unittest

import compare


def result(error, code=None):
    body = {"error": {"code": code}} if error else {"id": "1"}
    return {"isError": error, "contentCount": 1, "json": body, "structured": "same-as-text"}


def trace(*results):
    calls = [{"tool": "t", "group": 1, "compare": "isError-set", **item} for item in results]
    return {"startedAt": "2026-01-01T00:00:00Z", "finishedAt": "2026-01-01T00:00:01Z", "calls": calls}


def problems(left, right):
    pairs = zip(compare.normalized_calls(left, set()), compare.normalized_calls(right, set()))
    return [p for (call, a), (_, b) in pairs for p in compare.compare_call(call, a, b, False)]


class ErrorSet(unittest.TestCase):
    def test_a_double_claim_is_a_difference(self):
        self.assertTrue(problems(trace(result(False), result(True, "taken")), trace(result(False), result(False))))

    def test_all_errors_is_a_difference(self):
        self.assertTrue(problems(trace(result(False), result(True, "taken")), trace(result(True, "taken"), result(True, "x"))))

    def test_the_winner_may_vary(self):
        self.assertEqual(problems(trace(result(False), result(True, "taken")), trace(result(True, "other"), result(False))), [])

    def test_only_the_flag_counts_within_a_group(self):
        self.assertEqual(problems(trace(result(True, "a"), result(False)), trace(result(False), result(True, "b"))), [])

    def test_a_flag_outweighs_any_other_field_when_sorting(self):
        # contentCount sorts before isError in the JSON, so sorting on JSON alone would misalign these.
        left = trace(result(False), {**result(True, "a"), "contentCount": 0})
        right = trace({**result(True, "a"), "contentCount": 2}, result(False))
        self.assertEqual(problems(left, right), [])

    def test_an_http_failure_is_not_a_tool_error(self):
        self.assertTrue(problems(trace(result(False), result(True, "a")), trace(result(False), {"httpStatus": 500})))


class IsError(unittest.TestCase):
    def test_a_flipped_flag_is_a_difference(self):
        left = {"startedAt": "2026-01-01T00:00:00Z", "finishedAt": "2026-01-01T00:00:01Z",
                "calls": [{"tool": "t", "compare": "isError", **result(True, "a")}]}
        right = {**left, "calls": [{"tool": "t", "compare": "isError", **result(False)}]}
        self.assertTrue(problems(left, right))


class BatchResults(unittest.TestCase):
    def wrap(self, *items):
        call = {"tool": "t", "compare": "batch-results", "isError": False, "json": {"results": list(items)}}
        return {"startedAt": "2026-01-01T00:00:00Z", "finishedAt": "2026-01-01T00:00:01Z", "calls": [call]}

    def test_a_prisma_message_matches_any_error_only(self):
        prisma = {"error": "x invocation in\ny"}
        self.assertEqual(problems(self.wrap(prisma), self.wrap({"error": "other"})), [])
        self.assertTrue(problems(self.wrap(prisma), self.wrap({"id": "1"})))

    def test_other_messages_and_lengths_stay_exact(self):
        self.assertTrue(problems(self.wrap({"error": "a"}), self.wrap({"error": "b"})))
        self.assertTrue(problems(self.wrap({"id": "1"}), self.wrap({"id": "1"}, {"id": "2"})))


class Modes(unittest.TestCase):
    def generated(self, size, company="Acme"):
        return {"isError": False, "json": {"size": size, "company": company}}

    def test_a_generated_pdf_may_differ_in_size_only(self):
        call = {"compare": "pdf-size"}
        self.assertEqual(compare.compare_call(call, self.generated(10), self.generated(20), False), [])
        self.assertTrue(compare.compare_call(call, self.generated(10), self.generated(20, "Other"), False))

    def test_the_tool_list_may_only_add_the_additive_tools_at_the_end(self):
        additive = json.loads((compare.HERE.parent / "contract" / "mcp-tools.additive.json").read_text())["tools"]
        call = {"compare": "tools-prefix"}
        first = {"json": {"tools": [{"name": "a"}, {"name": "b"}]}}
        good = {"json": {"tools": first["json"]["tools"] + additive}}
        self.assertEqual(compare.compare_call(call, first, good, False), [])
        reordered = {"json": {"tools": [{"name": "b"}, {"name": "a"}] + additive}}
        self.assertTrue(compare.compare_call(call, first, reordered, False))
        extra = {"json": {"tools": first["json"]["tools"] + additive + [{"name": "c"}]}}
        self.assertTrue(compare.compare_call(call, first, extra, False))
        before = {"json": {"tools": additive + first["json"]["tools"]}}
        self.assertTrue(compare.compare_call(call, first, before, False))


if __name__ == "__main__":
    unittest.main()
