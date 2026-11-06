# Nexus skill corrections

The handoff treats the two Nexus skills as the de facto spec, so they must track the server. QA ran their workflows against the new server and found these passages wrong. Most had already drifted from old Nexus before the rewrite.

These changes haven't been applied: the skills live on Hermes, and the owner applies them. The findings come from system QA on 2026-11-06. QA read old Nexus's code for items 1, 2, 3 and 8, and it behaves like the new server, so the skills had drifted before the rewrite. Items 4 to 7 were checked on the new server only, with the golden traces covering the same tools.

**`nexus-job-intake-interview-pipeline/SKILL.md`**
1. **Common Pitfalls, "After `interview_scheduled`/`interview_completed` the previous `followUpAt` stays".** Half wrong. `interview_scheduled` sets `followUpAt` to `scheduledAt`. `interview_completed` keeps it unless its metadata has `followUpAt`.
2. **Pitfall 18, `jobContentHash`.** `jobContentHash_invalid` comes from create and upsert. `update_application` returns `application_update_failed` for the same value, and a batch returns `Batch upsert failed`. Uppercase isn't rejected: it's lowercased. `docs/SPEC.md` section 4 has the same row.
3. **Pitfall 3 and "Cleanup / pruning", "clear `appliedAt`".** No tool can. `update_application` and `batch_upsert_applications` both return `lifecycle_event_required`, even for null. Replace with: to record an interview with no confirmed application, create as `inbound`, then record `interview_invited`, which sets `status=interview` and leaves `appliedAt` null. Creating directly at `applied`, `interview`, `offer` or `rejected` always sets it. Also change "Clear stale `followUpAt` for rejected records": `application_rejected` already does.
4. **Step 9, submission.** Add that `record_application_submission` needs `policy`: `humanReviewed`, `identityConsistent` and `factsVerified` all true, and `profileConsistencyStatus` of `verified` or `unavailable_reviewed`. It also needs at least one document, plus answers or `confirmedNoAnswers: true`. Same-company and repeat submissions need the override reasons. No skill mentions any of this, and an agent's first attempt fails with `human_review_required`.
5. **Pitfall 18 (second), "If `list_application_events` requires... `mcp:submissions`".** It doesn't: a tools-only token can read and record events. It's needed for submissions, recall, submitted documents and deleting applications with submissions.
6. **Pitfall 15, local offsets.** Be more exact: `lastContact`, `followUpAt`, `occurredAt`, `submittedAt` and the `job*At` fields reject an offset, while `appliedAt`, `archivedAt` and the event metadata's `followUpAt` and `scheduledAt` accept and normalize it. A bare date also passes in metadata, as midnight UTC, so always send a full `Z` timestamp.
7. **MCP note on `source`.** Exact now: `create_application`, `update_application` and contact `role` truncate to exactly 100, dropping a whole emoji at the edge. Upsert and document metadata reject over 100.

**`references/recruiter-email-invite-intake.md:36`, `recruiter-consultora-screening.md:17`, `anonymous-agency-client-identification.md:72`**: the same fix as item 3.

**`references/application-submission-package.md`**
8. **"Idempotent note/event append... atomic SQL concatenation".** `append_application_note` writes only a `note_added` event and doesn't change `notes`. Also remove what no longer applies: all Firestore and Prisma adapter text, the "Product-development workflow" section, the GCS assumptions, and "Ideal MCP operation" (the tool exists; its response keys are `replayed`, `dryRun`, `verified`, `application`, `submission`, `event`, `documents`).

**`references/mcp-post-deploy-smoke.md`**
- **"Pace MCP calls below the general route limit (30 requests per 60 seconds)".** The new server has no such limit: 4,000 calls a second got through locally. It answers saturation with its pools instead. Keep the advice to respect 429 and `Retry-After`. Also change "every database adapter", as there's now one.

**`career-lead-discovery-nexus/references/daily-cron-tooling-patterns.md`**
- **Section "appliedAt auto-population on inbound leads".** Delete it. Batch doesn't set `appliedAt` on an inbound lead, and its repair call returns `lifecycle_event_required`.
- **Step 4, partial batches.** Add: an unknown `id` anywhere fails the whole call with `Batch upsert failed` and applies nothing, while lifecycle and URL conflicts fail per item.
- **"`MCP error -32602: Required path: [items]`".** The real text is a JSON issue list with `"path": ["items"]` and `"message": "Required"`. Cosmetic, but agents matching on it will miss it.
- **`/opt/data/nexus_refresh.py` and `nexus_mcp_call.py`.** I couldn't read these. They hold a client registration and tokens, which end at cutover: the tables start empty, so they must re-register with a loopback or allowed redirect.

**Confirmed accurate, no change:** mixed lifecycle updates failing atomically, `idempotencyKey` needing `occurredAt`, outcome limit 255, omitted `remote` reading `false`, `{id, nextAction: null}`, `status` as an array, `items` as the parameter, event argument names, archived records keeping `inbound`, closure events setting `lastContact`, strict event metadata, `companySize: null` accepted, batch result shape.

**Not checked:** `resume-tailoring` and `job-application-tracking` weren't in the skills I was given, so the SPEC's open item about `/resume` and `/tailor` is still open.
