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
- **`/opt/data/nexus_refresh.py` and `nexus_mcp_call.py`.** QA could not read these. They hold a client registration and tokens, which end at cutover: the tables start empty, so they must re-register with a loopback or allowed redirect.

**Every skill or script that counts Nexus's tools**
- The server now lists 41 tools, not 35: the same 35 in the same order with the same schemas, then `get_cv_source`, `patch_cv`, `render_cv_pdf`, `list_cv_revisions`, `revert_cv` and `create_document_upload`. A check for "35 tools" should become "at least these 35", because a client that compares the whole list will fail.

**`resume-tailoring`, `job-application-tracking` and anything else that produces a CV**
- Typst replaces Reactive Resume once the owner has moved real applications across. Until then Reactive Resume stays usable. Don't remove its steps before the owner says so.
- `generate_tailored_cv` now works. Its response is the same, but with locked facts it can answer `{"error":{"code":"cv_approval_required","variant":...,"revision":...,"approvalPath":...}}`: the text is a draft, the revision is stored, and no document exists. Tell the owner the path and stop. After they approve, the same call stores the document. It can also refuse with `cv_fact_violation`. Report it and stop. Don't reword around it. A generate from the owner's wording, unchanged since the last lock and with no `profileOverride`, needs no approval.
- `render_cv_pdf` gives a draft as stamped PNGs with no PDF. Saving a draft to an application answers `cv_approval_required`.
- Text the owner has replaced with a newer approval stops being approved in that variant. `revert_cv` to it, `render_cv_pdf` naming its old revision, and `generate_tailored_cv` from wording put back all give a draft, `cv_approval_required` where a document was asked for. An undo back to the newest approved text is approved at once, with no tap.
- `patch_cv` refuses `origin` values `dashboard`, `revert`, `cli`, `lock` and `voice` as `cv_origin_reserved`, and anything but lowercase letters, digits, `-` and `_` as `cv_origin_invalid`. `telegram` is fine. A `generate_tailored_cv` naming a job or category twice, or no job at all, is a draft even over a lock.
- `upsert_cv_profile` can refuse with `cv_facts_locked` once the owner has locked the facts. Its `fields` say what would have changed. Only the owner can change facts: `nexus cv-facts unlock` on the board. Wording changes go through, numbers included, and a CV generated from them is a draft.
- **A rule for every skill that submits an application: a submission must only use a document made from an approved revision.** That means a document stored by `generate_tailored_cv` or `render_cv_pdf` with `saveToApplicationId`. `upload_document_content` is outside the gate, so never upload a CV made elsewhere and submit that. Never call a CV final, ready or approved until a tool says `state: "approved"`.

**Every skill that uploads a document**
- An agent with a shell should upload any file over a few MB through `create_document_upload`: call it with `filename`, `mimeType` and optional `applicationIds`, then run the `example` it returns with the real file path, `curl -fsS -T <file> '<uploadUrl>'`. The reply is the stored document, as `upload_document_content` returns it. The link works once and for 15 minutes, and takes up to 50 MiB. On a 404 mint a new link: a failed upload uses its link up too. `upload_document_content` stays for small files and agents without a shell, up to 10 MiB as base64.
- The rule above holds here too: a file uploaded through a link is outside the CV approval gate, so never upload a CV made elsewhere and submit that.

**A new skill section for dictation, for the Hermes agent on Telegram.** The full text and the reasons are in [`DICTATION.md`](DICTATION.md). In short:

> When the user sends a voice note about their CV: transcribe it, call `get_cv_source`, then `patch_cv` with `origin: "telegram"` and the `revision` you read as `expectedRevision`.
>
> On `cv_compile_failed`, read the `diagnostics`, fix your source and call `patch_cv` again. Don't bother the user with it. On `cv_edit_no_match` or `cv_edit_ambiguous`, fix your `oldText`. On `cv_revision_conflict` or `cv_facts_changed`, read the source again and reapply.
>
> On success the edit is a **draft**. Call `render_cv_pdf` and send the PNG, which is stamped `DRAFT, NOT APPROVED`. Then name the most important `reviewItems`, which come most serious first, and give the `approvalPath` on the owner's dashboard. You can't tell whether those items are true: ask the user to check them on screen and approve there. **You can't approve, and no tool can. Never say the CV is final, ready or approved until `state` is `approved`.**
>
> On `cv_approval_required`, nothing was stored as a document. Give the user its `approvalPath` and stop. After they approve, the same call goes through.
>
> On `cv_fact_violation`, tell the user exactly which `token` was refused: a reader would see something other than what they review, or it is a phrase they banned. Never work around it: don't reword it, spell it differently, split it, or try another tool. Nothing was saved. On `cv_facts_not_locked`, tell the user to run `nexus cv-facts lock`, read what it prints, and confirm it with the `--confirm <hash>` it names.
>
> On `cv_render_busy`, wait 10 seconds and retry, at most 3 times. If a call gets no answer at all, it may have stored its revision: call `get_cv_source` and see, before you apply the edit again.

**Confirmed accurate, no change:** mixed lifecycle updates failing atomically, `idempotencyKey` needing `occurredAt`, outcome limit 255, omitted `remote` reading `false`, `{id, nextAction: null}`, `status` as an array, `items` as the parameter, event argument names, archived records keeping `inbound`, closure events setting `lastContact`, strict event metadata, `companySize: null` accepted, batch result shape.

**Not checked:** `resume-tailoring` and `job-application-tracking` were not available to QA, so the SPEC's open item about `/resume` and `/tailor` is still open.
