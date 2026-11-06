# Nexus rewrite: spec

Status: M1 built, gate open. M2 built on top of it (section 9a). M3 designed in section 9b.
Baseline: `nexus-crm` at `235138d`, the commit in the running production image's tag on 2026-11-06. Sources marked **[handoff]** are not yet checked against the code or the Nexus skills.

## 1. Goal

Replace the Next.js Nexus with one Rust binary on a ROCK 4 SE: an MCP tool server for agents, with a read-only web dashboard for humans. The live system stays the system of record until the M1 gate passes.

## 2. Decisions

| Decision | Choice | Consequence |
|---|---|---|
| CV rendering | Typst, from M2. M1 has no renderer | `CvProfile` and `CvPatch` are empty in production and no stored CV came from `generate_tailored_cv`. See section 4. |
| Data storage on the board | Root filesystem, outside `/home`, with encrypted off-site backups | Nexus restarts unattended after a reboot. Data sits unencrypted on the NVMe if the board is taken. |
| Host | ROCK 4 SE, subject to the Ethernet check in section 10 | |
| Auth | Keep OAuth 2.1 in v1, alongside API tokens | Port the authorization server, consent flow and a login page to Rust. Needs a public endpoint. |
| Scope | Single user. Drop email scanning, assistant tables, share links, demo workspace | `ScannedEmail` has no rows in production, so there is nothing to archive or import. |
| Hosting of this repo | Private, on Forgejo | Spec and integrity rules stay off public hosts. |

Reactive Resume stays usable for real applications until M2 **[handoff]**. Typst source editing (`patch_cv` and friends) also stays in M2.

## 3. Architecture

One axum process serves all of these:

| Path | Purpose |
|---|---|
| `/api/mcp`, `/mcp` | MCP over Streamable HTTP (`/api/mcp` is the current path) |
| `/.well-known/oauth-*`, `/api/mcp/{authorize,token,register}` | OAuth 2.1 |
| `/api/*` | REST, for the dashboard and later the TUI |
| `/calendar.ics` | Interviews and follow-ups |
| `/` | Leptos SSR dashboard, read-only in M1 |

Workspace: `core` (domain, SQLite), `mcp`, `web`, `tailor` (Typst, M2), and `tui` (M4), plus the binary.

| Crate | Latest stable on 2026-11-06 | Minimum Rust |
|---|---|---|
| axum | 0.8.9 | 1.80 |
| sqlx | 0.9.0 | 1.94 |
| rmcp | 3.9.0 | 1.88 |
| leptos / leptos_axum | 0.8.23 / 0.8.10 | 1.88 |
| typst, typst-pdf, typst-render, typst-kit (M2) | 0.15.1 | 1.92 |
| ratatui (M4) | 0.30.2 | 1.88 |

The local toolchain (1.99.1) covers all of these. Versions come from crates.io; API details have not been checked, because Context7 was down.

Build: cross-compile to `aarch64-unknown-linux-musl` so the binary doesn't depend on the board's OS image. Deploy with systemd.

## 4. MCP contract

`contract/mcp-tools.snapshot.json` holds real `tools/list` output for all 35 tools, generated from source at `235138d`.

1. **Serve the frozen schemas verbatim.** Embed the snapshot's `inputSchema` values instead of generating them from Rust types. Generated schemas would drift in small ways, such as `nullable` encoding and `$schema`. The golden contract test confirms the served schemas match.
2. **Golden test.** CI compares the server's `tools/list` with the snapshot, byte for byte and in order, then the rest of the list with `contract/mcp-tools.additive.json`. Any difference fails the build. A client now sees 40 tools: the 35 frozen ones, then 5 additive.
3. **Keep the quirks.** Limits differ per tool, and clients may depend on that:
   - `source` has `maxLength: 100` on `upsert_application_by_job_url` and `update_document_metadata`, but no limit in the schema on `create_application` or `update_application`.
   - `rating` is an integer on create and upsert, but a number on update.
   - `update_application` advertises lifecycle fields, then rejects them.
4. **Lifecycle rule.** `update_application` with any of `status`, `appliedAt`, `lastContact`, `followUpAt` or `currentStage` returns `{"error":{"code":"lifecycle_event_required","fields":[...]}}` as a tool error, and nothing is written.
5. **Confirm against production.** The snapshot was generated from the deployed commit, not read from the running server. Compare it with a live `tools/list` once before freezing.
6. **`generate_tailored_cv`.** The tool keeps its frozen schema and response. With no profile it returns old Nexus's exact tool error, `No CV profile found. Use upsert_cv_profile first.`. With one, it renders with Typst as section 9a describes. M1 alone returned a tool error here, and that gap is closed: what remains different is the PDF's bytes and so `size`, and that the document carries a `contentHash` where old Nexus left it null. Two new errors old Nexus could not return, `cv_fact_violation` for what a reader would see otherwise than the owner reviews, and `cv_approval_required` once the facts are locked and the wording isn't what the owner approved, can't affect current data, because production has no profile.

Runtime behaviour the schemas don't show is covered by the parity scenarios in `parity/`. The `nexus-job-intake-interview-pipeline` skill listed these from real use. This table is what the parity run against old Nexus's code at `235138d` found:

| Behaviour | Expected |
|---|---|
| Mixed metadata and lifecycle call | Fails atomically, applies nothing |
| `idempotencyKey` without `occurredAt` | Rejected with `occurred_at_required_for_idempotency` |
| Event `metadata.outcome` over 255 chars | Rejected, though the generic metadata schema doesn't show the limit |
| Document `source` over 100 chars | Schema error |
| Application `source` over 100 chars | Truncated silently to 100 UTF-16 units by `create_application`, `update_application` and batch. `upsert_application_by_job_url` rejects it by schema |
| `jobContentHash` not 64 hex chars | `upsert_application_by_job_url`: `{"error":{"code":"jobContentHash_invalid"}}`. `create_application`: plain text `jobContentHash_invalid`. `update_application`: `application_update_failed`. `batch_upsert_applications`: the whole call fails with `Batch upsert failed`. Over 64 characters is a schema error. Uppercase hex is accepted and stored lowercase; `""` and `null` store null |
| Local-offset timestamps | Depends on the field. Schema error: `lastContact`, `followUpAt` on create, `occurredAt`, `submittedAt`, and `expectedUpdatedAt` on update and events. Converted to UTC: `appliedAt` on create, `archivedAt`, `expectedUpdatedAt` on archive and unarchive, and metadata `scheduledAt` and `followUpAt`. Clients should send `Z` |
| Omitted `remote` | Reads back as `false` |
| Creating a record at `applied`, `interview`, `offer` or `rejected` with absent or `""` `appliedAt` | Any create path sets it to the current time. `inbound` stays null. An explicit `appliedAt` is always kept |
| Event tools without `mcp:submissions` | No scope needed: `list_application_events` and `list_application_activity` are allowed, including `application_submitted` events. Submissions, recall, `pipeline_healthcheck`, submission documents and deleting applications are blocked with `insufficient_scope` |
| Submissions | Concurrent idempotent replay, atomic document claims, immutability, aggregate payload limits, scope enforcement on exact answers |

## 5. Data model

SQLite (WAL, foreign keys on) with blobs on the filesystem.

| Prisma model | New table | Changes |
|---|---|---|
| `Application` | `applications` | Drop `userId`, `isDemo`, `demoWorkspaceId`, `demoKey`. `canonicalJobUrl` becomes unique on its own. |
| `ApplicationEvent` | `application_events` | Same drops. `idempotencyKey` becomes unique on its own. Keep `contactId` as text, as it is today. |
| `Contact` | `contacts` | None |
| `Document` | `documents` | Drop `userId`, `demoProvenance` |
| `_ApplicationToDocument` | `application_documents` | Explicit join table |
| `ApplicationSubmission` | `application_submissions` | Drop `userId` |
| `CvProfile` / `CvPatch` | `cv_profile` / `cv_patches` | Profile becomes a single row |
| `UserApiToken` | `api_tokens` | Starts empty: production has no tokens, so every current client uses OAuth |
| `McpOAuthClient`, `McpAuthCode`, `McpAccessToken`, `McpRefreshToken` | `oauth_*` | Start empty. Arrays become JSON text. |

Rules:
- **Keep integer IDs exactly.** Notes, skills and cron jobs refer to applications by ID.
- JSON columns become `TEXT CHECK (json_valid(...))`. Timestamps are UTC ISO 8601 with `Z`.
- Events can't be updated (`BEFORE UPDATE` trigger raises). There is no delete trigger, because `delete_application` and `batch_delete_applications` must still cascade as they do today, and SQLite fires triggers on foreign-key cascades.
- Documents linked to a submission are immutable, carrying over `isSubmissionDocument` from `lib/documents/access`.
- Blobs live at `blobs/<sha256>`. Check every stored `contentHash` during import.
- `documents.blob_sha256` locates the file in `blobs/`. It is separate from `content_hash`, which clients can overwrite or clear, so no lookup uses `content_hash`.

Dropped: `User`, `Session`, `Account`, `Verification`, `ShareLink`, `DemoWorkspace`, `AdminAuditLog`, `EmailIntegration`, `ScannedEmail`, `LlmCredential`, all `Agent*`, `ActionProposal`, `BulkCommand*`.

## 6. Auth

- Port `lib/mcp-oauth.ts` and `app/api/mcp/{authorize,register,token}`: dynamic client registration, PKCE S256, single-use codes, hashed tokens, refresh rotation, scopes `mcp:tools` and `mcp:submissions`, and the sensitive-consent version.
- Token routing stays by prefix: `mcp_at_` is OAuth, anything else is checked as an API token.
- better-auth goes away, so `authorize` needs a new login: one owner passphrase (argon2id) with rate limiting, a session cookie and the existing consent step from the `authorize` route.
- rmcp's `auth` features look client-oriented, so assume the authorization server is hand-written in axum. To verify at scaffold time.
- Hermes Career Ops also connects with OAuth (`auth: oauth` in its config), not only Claude.ai.
- Registration is open, because Claude.ai registers itself. So a redirect URI must be https to a host in `NEXUS_OAUTH_REDIRECT_HOSTS` (default `claude.ai,claude.com`, Claude.ai's real host still unverified) or loopback (`127.0.0.1` or `localhost`; `http://[::1]` stays refused, as in old Nexus), checked at registration and again at authorize.
- Rate limits and login backoff key on the client's address. With an https base URL, `serve` refuses to start until `NEXUS_TRUSTED_IP_HEADER` names the ingress's client-address header or says `none`. The header is believed only from `NEXUS_TRUSTED_PROXY_PEERS` (default loopback).
- OAuth clients re-register after cutover if the issuer URL changes, so Claude.ai and Hermes would each need one reconnect. Keeping `nexus.vasudev.xyz` doesn't avoid this unless tokens and clients are migrated, and the OAuth tables start empty. Plan one re-authorization at cutover.

## 7. Migration

**Export, from the live system (read-only):**
1. **Live data location, read from the running container on 2026-11-06:** Nexus runs on `hetzner-hel` (Coolify). `DB_PROVIDER` is unset, so it uses Prisma. `DATABASE_URL` points at a Coolify `postgres:17-alpine` container on the same host, not Neon or Firestore. `GCS_BUCKET` is set and credentials are configured, so document blobs are in GCS. Neon's role, if any, is unconfirmed: check it before relying on the Neon project.
2. Run `pg_dump` inside that container with a **table allowlist**: `Application`, `ApplicationEvent`, `ApplicationSubmission`, `Contact`, `Document`, `_ApplicationToDocument`, `CvProfile`, `CvPatch`, `ScannedEmail`. Never dump credential tables (`LlmCredential`, `EmailIntegration`, `Session`, `Account`, `UserApiToken`, `Mcp*`).
3. Fetch blobs from GCS with the app container's own client, so its service-account key stays on the server.
4. Store outside any repo, in a mode 700 directory.

**First export, 2026-11-06 (from `235138d`):**

| Table | Rows |
|---|---|
| `Application` | 190 |
| `ApplicationEvent` | 205 |
| `Contact` | 36 |
| `Document` | 81 |
| `_ApplicationToDocument` | 56 |
| `ApplicationSubmission` | 1 |
| `CvProfile`, `CvPatch`, `ScannedEmail`, `UserApiToken`, `DemoWorkspace` | 0 |

All rows belong to the owner, and none are demo rows. The other 11 `User` rows own nothing.

**Known data gap: this export is not complete.**
- 49 of 81 documents have their file; every stored `contentHash` among them matched.
- 32 documents (uploaded 2026-06-23 to 2026-07-10, all PDFs, type `other`, none linked to a submission, 15 linked to applications) have no file in the bucket, its pre-deploy backups, other visible buckets, GCS versions, or the `hetzner-hel` disks searched.
- Likely cause, inferred and not proven: commit `4624b87` added a fallback to local disk when GCS credentials are missing, and the container has no uploads volume, so a redeploy would have erased them. Found files start the day the missing ones end.
- Candidates recovered from Hermes data into quarantine: 21 match on size and name, 5 on size only, 6 have none. These rows have no stored hash, so identity can't be proven. They need human confirmation before import, and they stay out of `blobs/`.

**Importer rules from this data:**
- `contentHash` is null on 35 rows, including 3 with files. Compute it for internal integrity and record that it was computed, but keep returning null to clients until one writes a value, as old Nexus does.
- Unconfirmed rows import with `blob_state = 'missing'`, not as if the file exists.
- `get_document` returns normal metadata for them, and only `download_document_content` fails, both as in old Nexus (confirmed by the parity baseline).

**Response compatibility.** Responses keep the fields clients see today even though the model is single-user: `userId` is the imported owner's ID, `isDemo` is false, `demoKey` and `demoWorkspaceId` are null, and `filename` is unchanged.

**Documented difference.** Above 1 MiB, production's `download_document_content` returns a signed GCS URL. The new server has no object store and returns content inline.

**Old Nexus backups.** The bucket also holds `_nexus-backups/pre-deploy/` (62 snapshots, 53 objects) and 14 unreferenced root objects. Review these for retention and secrets when decommissioning.

**Import:**
- One idempotent command that loads the owner's rows only and skips `isDemo` and `demoProvenance` rows.
- It fails on row count, hash or foreign-key mismatches.

**Parity gate:**
- For every application: `get_application`, `list_application_events`, contacts, documents and submissions return the same content from both servers, allowing only for documented differences.
- Writes go to a copy, never to live: replay recorded calls and compare responses and readbacks.

**Parity result, 2026-11-06, M2.** Golden scenarios of 1,809 calls through the real `nexus serve` binary, compared with old Nexus at `235138d` restored locally: 1,808 pass, 1 fail (`sort: company`), none documented. `generate_tailored_cv` now compares exactly except `size`, and `tools/list` must be the frozen list followed by exactly the additive contract. The M1 run had 1,805 pass and 3 documented for the same calls. That one is an artifact of the baseline: it ran under glibc `en_US` collation, while production compares bytewise (`'Z' < 'a'` is true there), as Rust does. Contact and linked-application order is compared unordered, because old Nexus returned Postgres heap order, which no client can rely on. About 18,000 further fuzzed calls matched apart from explained differences. The baseline goes stale for time-dependent `pipeline_healthcheck` checks from 2026-11-09, so re-record it after that.

**Cutover:** stop writes to old Nexus, re-export, import, re-run parity, then repoint clients. Keep old Nexus stopped but restorable for 30 days. The runbook is [`CUTOVER.md`](CUTOVER.md).

## 8. Deployment and open risks

**Board, read on 2026-11-06.** ROCK 4 SE, 6 cores, aarch64, Armbian 26.11.0-trunk (Debian 13), kernel 6.18. Armbian trunk is a development image, so expect kernel changes. RAM is 3.8 GiB with about 1.8 GiB available, alongside Coolify, Forgejo, CLIProxy and Joplin. NVMe root has 66 GiB free. `/home` is on a LUKS partition that stays locked after reboot, and was locked when read.

**Storage.** SQLite and blobs live on the root filesystem under `/var/lib/nexus`, never in `/home`, so the service survives an unattended reboot.

**RAM.** Target 30-60 MB at idle **[handoff]**. Measured on the board on 2026-11-06 at `0d5b5d8` with synthetic fixtures: 6.8 MB after start, 12.7 MB after 500 requests including 200 MCP calls, tool calls about 10 ms, binary 12.8 MB. With the real data set on x86-64, an earlier measurement before MCP was merged: 7.5 MiB after start, 29 MiB peak under 16 parallel clients. At `ab910ed` with `synchronous=FULL`, also on the board with synthetic fixtures: 13.2 MB peak, and 100 sequential `create_application` calls took 46.6 ms each including `curl` start-up. The real data set hasn't been measured on the board. M2 was measured on the board on 2026-11-06 at `3ca11bb` with synthetic data, arm64 musl binary 43.4 MB. The render worker peaked at 16.5 MB for text only, 36.7 MB for 2 pages with PDF and 1 PNG, 38.4 MB with 2 PNG, and 48 MB for 9 pages with 9 PNG. `serve` used 10.0 MB idle and 11.3 MB after 20 requests. Timings haven't been measured there yet.

**Backup.** `nexus backup` writes one age-encrypted file from a `VACUUM INTO` snapshot and the blobs it references, on a systemd timer (`deploy/README.md` section 6). The off-site destination is undecided.

**Durability.** `Db::open` sets `synchronous=FULL`, because an acknowledged submission must survive power loss: under `NORMAL`, a reboot can silently undo writes an agent already reported saved. QA's cost, on an x86-64 desktop NVMe (btrfs, 2 pinned cores) through real MCP, `NORMAL` then `FULL`: one agent p50 0.59 then 1.60 ms, p99 6.15 then 17.84 ms, 1,324 then 341 writes a second. Eight agents flat out: p50 1.65 then 11.72 ms, p99 34.50 then 333.20 ms, 2,430 then 348 writes a second, slowest single write 5.4 s against a 10 s busy timeout. Reads are unchanged. The board has not been measured, and real load is a tiny fraction of this. FULL does not cover a drive that lies about flushes. Real power loss has not been tested. `journal_size_limit` is 64 MiB: a checkpoint truncates a WAL grown past that once it restarts. The WAL can still grow beyond it under sustained load.

**Ingress: must be tested, not assumed.** Claude.ai needs a public URL.
- A Cloudflare tunnel keeps a stable custom domain, but local notes say the Cloudflare proxy breaks MCP streaming unless buffering is disabled.
- Tailscale Funnel avoids Cloudflare, but the hostname becomes `*.ts.net`, so clients change URL once.
- M1 gate: a full Claude.ai OAuth and tool call through whichever one is chosen.

**Limits.** Each kind of caller has its own pool of request slots: public (login, OAuth, calendar: 8 in flight, 2 s wait, 10 s deadline), owner (dashboard, JSON API: 8, 10 s, 60 s) and mcp (8, 10 s, 60 s, or 300 s for a large body). A full pool gives a 503. On the MCP route, a body over 1 MiB or of unknown length, and any `download_document_content` or `render_cv_pdf`, also needs one of 2 large slots, because one 10 MiB document costs about 85 MB in the transport. At most 256 connections. Details in `deploy/README.md` section 7.

**Calendar.** Calendar apps can't send bearer tokens, so `/calendar.ics` needs a secret URL token. Google Calendar fetches subscriptions from its own servers and can't reach a tailnet-only URL. Use an on-device subscriber such as ICSx5, or publish the URL through the public ingress.

**Availability.** The board's logs for 2026-11-06 show a reboot at 00:15 (cause unknown), then the Ethernet link down from 00:26:39 to 00:49:07 (cause unknown) with the board still running. LAN, Tailscale and the Cloudflare tunnel all failed together, taking Forgejo and CLIProxy with them. The current host, `hetzner-hel`, stayed up throughout. Nexus would inherit this failure mode on the board. Agents need a clear error when Nexus is down, and no parallel tracker.

## 9. Milestones

| Milestone | Scope | Gate |
|---|---|---|
| M1 | Core, 35 tools (`generate_tailored_cv` per section 4), OAuth and tokens, REST, calendar, importer, read-only dashboard | Golden contract test, parity on real data, Claude.ai through real ingress, RAM measured on board |
| M2 | Typst renderer behind `generate_tailored_cv`; source and revisions: `get_cv_source`, `patch_cv`, `render_cv_pdf`, `list_cv_revisions`, `revert_cv`; Telegram dictation ([`DICTATION.md`](DICTATION.md)) | Locked facts have one writer, and no agent-written text reaches an output without the owner seeing it and approving exactly that text |
| M3 | Interactive PWA, push-to-talk, realtime session with a narrow toolset | Session can't mark anything `applied` |
| M4 | ratatui TUI | |
| M5 | Native wrap, only if the PWA proves painful | |

M2 note: `CvProfile.experience` mixes hard facts (dates, company) with wording (bullets) in one JSON column. The fact/wording split the integrity rule requires is a real schema change, not a rename.

M1's gate is still open (section 10). M2 and M3 are built on top of it, but none of this is deployed until the M1 gate has passed.

## 9a. M2 design: Typst and dictation

**Voice stays outside Nexus [handoff].** Tier 0 dictation is Telegram voice note → Hermes transcribes → Hermes calls `patch_cv` → Hermes sends the PNG preview back. Nexus has no Telegram code and no bot token. "Dictation works" means these tools plus the Hermes skill text in [`DICTATION.md`](DICTATION.md) and `SKILL-CHANGES.md`, tested by a scripted MCP client through real OAuth and the real render child (`crates/server/tests/cv_dictation.rs`). A real Telegram run needs the owner and the deployed server.

**`crates/tailor`, the `nexus-tailor` library.** It embeds Typst and owns rendering and the integrity check. No other crate touches Typst.

- **World.** Only three virtual files exist: `/main.typ` (the revision's source), `/facts.json` (what a CV can render: name, contact, employment, education, languages, metrics. Not `banned_phrases`, which a source could read back from the violation, nor `allowed_tokens` and `vocabulary`, which could then change under an approved revision: an approval's hash covers exactly what this file holds) and `/nexus.typ` (the read-only template library, embedded in the binary). No filesystem, network, packages or system fonts. `datetime.today()` returns none. Fonts are a small embedded OFL set, chosen for binary size.
- **Worker process.** Every compile runs in a child, `nexus __render` (same binary; handled before clap and the async runtime, so it stays out of `--help` and a compile runs in a plain process), with a cleared environment, JSON on stdin and a framed result on stdout. This keeps the server's RSS near M1's and lets a runaway compile die alone. The parent allows one compile at a time and kills the child on wall-clock timeout or when `/proc/<pid>/statm` RSS passes the limit. The child raises its own `oom_score_adj` to 1000, so the kernel takes it first. It doesn't call `setrlimit`: the unit's `SystemCallFilter=~@resources` forbids that. The unit gets `OOMPolicy=continue`. Tests in crates that can't name the binary use an in-process renderer, which is never used by `serve`.
- **Limits.** Source 256 KiB, 10 pages, PDF 10 MiB, 30 s wall clock. The RSS limit is 128 MiB. Its real ceiling is `MemoryHigh=256M`, not `MemoryMax`: above it the kernel kills nothing but stalls every task in the cgroup, the server included. So the limit, the server's own peak of about 85 MB and what a compile can allocate between two 20 ms polls (about 45 MB seen) must fit below it.
- **Diagnostics.** A compile error is data, not a failure: `{severity, message, file, line, column, endLine, endColumn, hints}`. Agents see it in the tool result. Users never do. The worker keeps the first 20 errors and 20 warnings, 5 hints each, and 1,000 characters of each text.
- **What crosses to the parent.** The worker refuses a PDF and previews over 12 MiB together and rendered text over 1 MiB as JSON, so the parent reads at most 14 MiB from it, and kills a child that sends more. The rendered text stays bounded because `#panic` of a long string, or a page of numbers, could otherwise fill the reply and the server. These replies are bounded with it: `cv_fact_violation` and `reviewItems` list the first 50, 200 characters of each text, with a `truncated` count, and one page of `list_cv_revisions` carries at most 1 MiB of diff.

**Data.** Migration `0004`:

| Table | Holds | Rules |
|---|---|---|
| `cv_facts` | Singleton. `name`, `contact`, `employment` (`id, company, title, date, location`), `education` (`id, institution, qualification, date, location, details`), `languages` (`name, level`), `metrics` (`key, text`), `allowed_tokens`, `banned_phrases`, `vocabulary`, `locked_wording`, `version`, `locked_at` | Hard facts. `allowed_tokens` are the URLs and emails a CV may show beyond the contact facts. `locked_wording` is the profile's wording as the last lock confirmed it. JSON columns are checked with `json_valid` |
| `cv_facts_history` | Each previous `cv_facts` state with who changed it: the owner's paths, and `upsert_cv_profile` as `changed_by: mcp` whenever an unlocked write changes a fact | Append-only, with triggers |
| `cv_profile` | `experience` keeps only `id, tier, bullets`. Name, contact, company, title, date, location and education move to `cv_facts` | `get_cv_profile` merges them, so a row in schema order reads back unchanged. A job always reads back in the order `id, company, title, date, location, tier, bullets`, so a row stored in another order, as Postgres `jsonb` does, comes back reordered, and `compare.py` ignores key order. Migration `0004` and the importer split any real row the same way, except that `0004` also gives a job or qualification without `location` or `date` empty ones. A fact in `0004` that isn't a string reads as empty and an entry that isn't an object is dropped, because `cv_facts` must stay readable: the importer refuses such a row instead. Education has no ID in old Nexus, so an entry's ID is its position (`edu-1`), which reordering it while unlocked changes. `degree` is stored as `qualification` |
| `cv_variants` | `id, name` (unique), `application_id`, `head_revision_id` | One variant per purpose. `default` is the base CV and always exists. `application-<id>` belongs to `generate_tailored_cv` and goes with its application |
| `cv_revisions` | `id, variant_id, number, parent_id, source, source_sha256, facts_version, origin, summary, warnings, created_at` | Immutable, enforced with triggers except for the foreign-key cascade when an application is deleted. `parent_id` has no foreign key, because a first revision's parent can be in another variant, which that cascade may remove. `warnings` holds the review items listed when it was written, capped |
| `cv_approvals` | `id, source_sha256, facts_sha256, rendered_text, variant_id, revision_number, approved_by, approved_at`, unique on `(source_sha256, facts_sha256)` | The owner's approvals. Append-only, with triggers, in every case: an approval outlives a variant that goes with its application, so it has no foreign key. `approved_by` is `cli`, `dashboard` or `lock`, by a `CHECK`. A revision's state is computed from here and stored nowhere else |

**The integrity gate.** Word and number rules can't tell a true sentence from a false one: an independent review put false dates, counts, titles and degrees through them with ordinary text, while they refused "Mentored junior developers". So the gate is this: **locked facts have one writer, and no agent-written text can reach an output without the owner seeing it and approving exactly that text.** Agents and voice edit wording freely. What they write stays a draft.

1. **Facts have one writer.** Nothing reachable from MCP or voice can change locked facts, or lock or unlock them. Owner-only paths lock, unlock or edit them: `nexus cv-facts` on the board and the dashboard's facts page, through the same functions in `repo::cv_facts`. Those record a `cv_facts_history` row, and so does an unlocked `upsert_cv_profile` that changes a fact, as `changed_by: mcp`, so that nothing an agent wrote in that window is lost.
   - `nexus cv-facts show` prints the facts to the owner's terminal.
   - `nexus cv-facts set --file facts.json` replaces the fields a profile can't express (`languages`, `metrics`, `allowedTokens`, `bannedPhrases`, `vocabulary`). Anything else in the file is refused. Employment, education, name and contact change through `upsert_cv_profile` while unlocked.
   - `nexus cv-facts lock` compares the facts and the profile's wording with the last lock and prints every changed field, old and new, quoted and escaped, and every URL in the wording that it would add to `allowed_tokens`. If there is anything, which is always so at a first lock, it writes nothing and exits non-zero, naming a hash of that list: `nexus cv-facts lock --confirm <hash>` then locks. If anything was written since, the hash no longer matches, and it prints the new list and refuses. So an agent writing between the printout and the command is seen, not confirmed. It refuses while no profile has set a name.
   - The dashboard's facts page shows the same list, and its form carries the same hash. A lock from a page older than the last write answers 409 and shows the new list.
   - `nexus cv-facts unlock` opens them again. While unlocked, every revision is a draft.
2. **`upsert_cv_profile` keeps its frozen schema.** While facts are unlocked, it writes facts and wording as old Nexus would, so parity holds. While locked, a change to any fact field or a new `experience.id` returns `{"error":{"code":"cv_facts_locked","fields":[...]}}`, and nothing is written. Leaving `education` out counts as a change, since old Nexus reads that as empty. Leaving a job out of `experience` doesn't: it changes the profile's wording only, and the job's facts stay. Wording is free, numbers included: it reaches no output before the owner approves it. An unlocked write that changes a fact moves `version`, so that a compile which raced it can't store.
3. **`patch_cv` and `revert_cv` need locked facts, and so does anything stored from `render_cv_pdf`.** Otherwise: `{"error":{"code":"cv_facts_not_locked"}}`. So an agent can't first rewrite the facts through the profile and then patch to match them. A plain `render_cv_pdf`, which stores nothing, needs no lock, and always gives a draft while unlocked. Neither does `generate_tailored_cv`, as old Nexus had none: see below.
4. **Sources refer to facts by id.** The template library renders `#employment("acme")`, `#education("x")`, `#languages()`, `#contact()`, `#metric("key")` from `/facts.json`.
5. **The check reads rendered output, not source.** Typst can compute text and rewrite it with show rules, so a source scan proves nothing. After a compile, the worker extracts every text run from the laid-out pages, including white, tiny, clipped and off-page text and the content of tilings, and what the PDF says beyond its pages: title, author, description, keywords, date, heading bookmarks with their numbering, link targets and page-label prefixes. PDFs are untagged, so alt texts carry nothing. `hide` is a compile error, because Typst drops hidden text from the frames altogether, so the check could never see it. Only `hide`'s own rule marks text hidden, and that rule is what fails: a source's show rule on `hide` can only replace it, so its text is then drawn and read, or dropped from the PDF too. `pdf.attach` is a compile error for another reason: the PDF exporter attaches every element it finds, whatever a show rule did to it, and an attachment is text no page shows. So after each compile the worker asks the document for attachments, as the exporter does. Headings and attachments are the only elements the exporter reads that way. `nexus_tailor::check` then sorts what it finds into two tiers.

**Hard refusals** are never overridable by approval, and nothing is stored. They are precise, and they exist because a reader of the PDF would see something other than what the owner reviews, or the owner would be approving text that isn't all there is. Compile errors are refused the same way.

| Rule | Refuses |
|---|---|
| Banned phrases | Owner's `banned_phrases`, case-insensitive after Unicode NFKC, lookalike folding, closing up spaced letters and whitespace folding, across line breaks. Real phrases live only in the database, never in this repo |
| Images | Any image in the document, because an SVG or PNG can carry text the check can't read |
| Hidden text | White or near-white, transparent, under 4 pt along either axis, off-page, clipped or tiling-painted text, even if its wording would pass. A glyph's ink must stand inside what shows, within 1 pt, not only its baseline. White text on a dark band is refused too |
| Bidi control | The characters U+202A to U+202E, U+2066 to U+2069, U+200E and U+200F in any run, read before normalizing removes them, and any run an override draws back to front. Typst trims a control from a line's ends while its effect stays, so the second test is the one that holds. Right-to-left scripts order themselves and pass |
| Transform | Text under anything but a translation and a uniform positive scale: rotated, mirrored, skewed or stretched, because a 6 turned is a 9. Likewise a glyph with a negative advance, because tracking draws `12` as `21`, a word space narrower than 0.15 em, because spacing draws `1 2` as `12`, and a run standing on another's text by more than 0.2 em, because a PDF reader joins them. Word and line gaps use the size as drawn, and a run of only spaces counts by the gap it leaves, so digits set close together read as one number |
| Contact details | An email-shaped text, also as `name(at)host`, or URL-shaped text (a scheme, `www.`, or a host in a listed top-level domain: `com org net info biz eu de at ch uk dev app xyz example test`) unless it is the contact email, a page under the contact LinkedIn or GitHub, in `allowedTokens` or `vocabulary`, or in a fact. A phone number with a `+` or `00` country code other than the contact phone. Any link target but `http`, `https` and a `mailto:` or `tel:` matching the contact facts. A link showing a contact value that goes anywhere else. Upper-case `.NET`, `Node.js` and `socket.io` are not URLs. A lock adds URLs in the profile's wording to `allowed_tokens`. This is the one decision `allowed_tokens` still takes part in |
| Digit lookalikes | A word only of `l`, `I`, `O`, `З`, `О` directly before `%` or a time unit reads as a number and is refused: `lOO%`, `ЗО%`, `lO years`. `IO`, `I/O` and `CI` elsewhere stay ordinary words |
| Mixed script | One word mixing Latin with Cyrillic or Greek lookalikes, which has no honest use on a CV. Umlauts, wholly Cyrillic or Greek words and hyphenated parts of different scripts (`Москва-Berlin`) pass |

**Review items** stop nothing. Each is `{rule, token, context, line}`, where `line` counts the lines of the rendered text from 1, and is null for what is no longer there. They are read on the lines that differ from the text last approved, because a line the owner has approved needs no second look, and a line that moved counts as changed, since a bullet moved to another job says something new. They come most serious first.

| Rule | Lists, on a changed line |
|---|---|
| `missingFact` | A company, institution, language, metric, or contact value on the last approved text and not on this one. Line null |
| `entityBinding` | A fact's company or institution named outside its own entry: its full name, the name without a legal form (`GmbH`, `AG`, `Ltd`, ...) or its first word as written, without its exact title or qualification and date on that line or the next two. A year or date within two lines of it that isn't its own, another entry's full date anywhere up to the next one, and another entry's title or location within two lines |
| `languageLevel` | A fact's language with a level other than its own: `German (C1)` when the fact says C2 |
| `claimWords` | Degree, seniority and language-level words, in any case and after folding lookalikes and spaced letters, including abbreviations with dots (`b.a.`) and stems (`*leitung`, `fließend*`). The words of a fact standing whole on that line and `vocabulary` entries are that fact's, and aren't listed |
| `numbers` | **Every** number and date, whether or not a fact has it: an allowed number in the wrong place was the review's finding. Any Unicode decimal digits count, normalized, as do CJK numerals, Roman numeral signs, tokens mixing letters and digits (`S3`), and months beside a year in any order (`Nov, 2019`, `March of 2019`, `2019 March`). True contact values are masked out |
| `numberWords` | Spelled-out counts and scale words, in English and German, anywhere (`eight`, `zweieinhalb`, `eight-plus`, `Tausend`), and upper-case Roman numerals within three words of a time unit |
| `newWords` | A word, in any case, in neither the facts, `vocabulary`, the wording last locked, the text last approved nor the template's headings. That closes finding 12: a lower-case `google` is listed, and small caps are read as they are written |

Each word is listed once per line, under the first rule to name it, so a date near a company shows as `entityBinding`.

**Approval.** A revision is `approved` or `draft`, computed, never stored on the revision and never passed by a caller. It is approved when `cv_approvals` holds its `(source_sha256, facts_sha256)` and the facts are locked.

- `facts_sha256` is a canonical hash of what `/facts.json` holds. `version` and `locked_at` aren't in it, nor `vocabulary`, `allowed_tokens` or `banned_phrases`, so an unlock and a relock with no change, or a vocabulary edit, invalidates nothing. Any changed fact invalidates every approval built on it.
- Text identical to approved text is already approved: `revert_cv` to it, `createFrom` of it, and running `generate_tailored_cv` again are approved at once. There is no re-approval loop.
- **Who can approve:** `nexus cv-facts approve --variant V --revision N --confirm` on the board, and a POST from the dashboard with an owner session and CSRF. Without `--confirm` the command prints the diff and the items and writes nothing. No MCP tool, no voice tool, no parameter: `crates/mcp/src/owner.rs` holds this as plain functions, and `crates/server/tests/cv_gate.rs` calls every listed tool with attack arguments and finds `cv_approvals` unchanged. `origin` is text a caller sends, recorded as data, and grants nothing.
- Approval needs locked facts, compiles again and re-reads the hard rules, and binds to the revision: it must be the variant's head, and the form carries the number, the source's hash and the facts' hash. A revision or a fact landing between page load and tap gets `cv_approval_stale`, and the page shows what is there now. An older revision is approved by reverting to it.
- **A save on the dashboard** is text the owner wrote and is looking at: it stores and approves in one transaction, in the web handler, through `owner::save_as_approved`, which no tool reaches. It can also store a variant's starting text as it stands.
- **The lock approves the owner's wording.** A lock records the profile's wording in `cv_facts.locked_wording`. A `generate_tailored_cv` whose source comes from just that wording, with no `profileOverride`, is approved by the lock: it writes the `cv_approvals` row on the owner's behalf, as `approved_by: lock`. A locked `upsert_cv_profile` that changes wording falls back to draft, which closes finding 12's laundering. This stores the wording itself rather than its hash, so that a lock can show what changed.

**What the owner reads.** The diff is against the last approved rendered text of that variant, not the parent revision, or a chain of small drafts would hide what changed. A first approval diffs against nothing, so the whole CV shows. It is rendered text, what a reader sees, one line each, then every link's target and what the PDF says beyond its pages, since those are output too. Control characters are removed from it, as they draw nothing and could rewrite a terminal. The source diff is available below it.

**The stamp.** A draft renders only with `DRAFT, NOT APPROVED` across every page, in PNG and any PDF. It is its own small Typst document, compiled in the worker and added as frames to each laid-out page, last, after the text is read. So nothing in a source reaches it: `#set page(background: none)`, `foreground`, a white rectangle or a show rule. A source can still choose what lies under it, so the stamp is translucent red with an opaque white outline: a page filled with the stamp's own red would hide one colour, and no fill matches both. And it isn't checked as CV text. `crates/tailor/tests/adversarial.rs` tests those attacks by comparing pixels with the same page unstamped. A preview's cache key holds its state, so a stamp can't fall off a draft or stay on what was approved.

**Every path.**

| Path | Facts locked, approved | Facts locked, draft | Facts unlocked |
|---|---|---|---|
| `patch_cv`, `revert_cv` | Stores, `state: approved` | Stores, `state: draft`, `reviewItems`, `approvalPath` | `cv_facts_not_locked` |
| `render_cv_pdf` | The real PDF and clean PNGs, `state: approved` | Stamped PNGs, no PDF, `state: draft`, `reviewItems`, `approvalPath` | As a draft |
| `render_cv_pdf` with `saveToApplicationId` | Stores a document | `cv_approval_required` | `cv_facts_not_locked` |
| `generate_tailored_cv` | Stores a document, frozen response | Stores the revision, **no document**, tool error `cv_approval_required` | As old Nexus, frozen response, and the stored PDF carries the stamp |
| Dashboard PDF download | The real PDF | The stamped PDF, named `cv-<variant>-<n>-DRAFT-NOT-APPROVED.pdf` | As a draft |

A hard refusal on any of these is `{"error":{"code":"cv_fact_violation","violations":[...]}}` and stores nothing. A revert or a render reads today's facts, so a revision that a hard rule now refuses, after a new banned phrase for instance, is a violation and not a stale PDF. A compile that raced a facts change stores nothing.

**Known limits.** The gate holds what it holds because a person reads the text. These stay:
- **The owner can approve something false.** That is the point of it being the owner. The highlights help, they don't decide, and a false sentence with no number and no new word in it shows only as a changed line.
- **`upload_document_content` is outside the gate.** An agent could upload a PDF made elsewhere and link it to an application. The frozen schema can't tell it from a real upload. `SKILL-CHANGES.md` carries the rule that a submission must only use a document made from an approved revision, which is an instruction, not an enforcement.
- **A stamped PNG can be forwarded.** So can a stamped PDF. The stamp says what it is, and that is all it can do. Someone could crop a corner of a picture or edit the PDF, since the stamp is drawn, not signed.
- **An approved source renders the same only for the same binary.** The key is source and facts, and a new Typst or a new template could draw the same text otherwise. `rendered_text` keeps what was approved, as data.
- **A review item is a highlight.** A date split over two lines is listed as its parts, and a word hyphenated at a line's end as two. A token a normalizer changed, like a dash in a date range, is listed but not marked in the diff.
- **A local-format phone number isn't refused.** It can't be told from a date range, so it is listed as a number. With a `+` or `00` country code it is refused.
- Text drawn with `curve` or many small shapes reads as nothing, like an image, isn't detected and shows on the stamped preview only. A white rectangle over true text hides it from the reader while the rendered text still has it, and text in the colour of a non-white background passes as visible. The owner reviews the stamped picture too for this reason.
- Lookalikes outside Cyrillic and Greek (Armenian, Latin IPA such as `ɑ` or `ı`) aren't folded. Digit lookalikes count as hard only directly before `%` or a time unit, so `Team of lO` is a new word, not a refusal.
- `http` and `https` link targets behind neutral text can point anywhere, because the wording an agent can edit supplies them. They are listed in the rendered text the owner approves, as `Link:` lines. A URL's host in an unlisted top-level domain, with no scheme and no `www.`, reads as an ordinary new word: `apply.evil.io`. An address split over two lines by something other than a space after `@` isn't seen.
- Text clipped by a box of its own height loses its descenders, and is refused when they are really cut. PDF named destinations from labels aren't read. No reader sees them unless a viewer lists them.
- `generate_tailored_cv` needs no lock, for parity with old Nexus, so while unlocked it can store a PDF built on facts an agent has just written. They show at the next lock, with their history, and the PDF carries the stamp.
- A lock approves all the wording it shows, for a generated CV. A long first lock is a long read, and anything it lists that the owner doesn't read is approved all the same.
- An approval on the board sends no live event: the page learns of it at its next reload, or its next poll if it has fallen back to polling. A dashboard approval reaches every open page at once.
- A parent killed outright leaves its child to systemd's cgroup kill. `OOMPolicy=continue` hasn't been exercised by a real kernel OOM kill.
- A client lost during a compile stops it, and nothing is stored. Lost after the compile, in the moments before the commit, the revision is stored with no one told, and a proxy may not pass on a disconnect at all. `expectedRevision` then refuses the retry, and `get_cv_source` tells.

**Tools.** Additive, in `contract/mcp-tools.additive.json`, checked by a second golden test. `tools/list` is the 35 frozen tools, in order, then these. All need `mcp:tools`.

| Tool | Behaviour |
|---|---|
| `get_cv_source` | `variant` (default `default`), optional `revision`. Returns source, revision number, `state`, facts without `bannedPhrases`, facts version and lock state, template-library help. A variant with nothing stored reads as revision 0, with a source generated from the master profile, or from the facts alone if there is none |
| `patch_cv` | `variant`, `expectedRevision` (required, 0 for nothing stored), `edits: [{oldText, newText}]` (applied in order; each `oldText` must match exactly once in the source as edited so far, or the call fails naming the edit), `summary`, optional `origin` (free text up to 64 characters, default `mcp`, recorded as data). Atomic. Compile failure returns diagnostics and stores nothing. Creates a variant with `createFrom`, the name of the variant to start from, whose head `expectedRevision` then names. Returns the revision number, `state`, `reviewItems` with `truncated`, `approvalPath` (a relative path, null when approved), a unified diff of the source against the parent, and the page count |
| `render_cv_pdf` | `variant`, optional `revision`, `previewPages` (default 1, at most 3), `saveToApplicationId`. Returns a JSON summary with `state`, `reviewItems`, `truncated` and `approvalPath`, and each PNG as MCP `image` content. An approved revision also gives the PDF as an MCP `resource` with a base64 blob, and `pdfBytes`. Saving needs an approved revision, and makes a normal document through the document store, linked to the application |
| `list_cv_revisions` | Paged, newest first (`page`, `pageSize` up to 50), each with its `state`, with `includeDiff` (unified diff against the parent, or against nothing if the parent is gone). Past 1 MiB of diff on a page, that revision and the older ones on it carry `diffOmitted: true` instead |
| `revert_cv` | `variant`, `toRevision`, `expectedRevision`. Adds a new revision with the old source: history never shrinks. Answers as `patch_cv`. Approved at once if that text is approved under today's facts |

**The write flow.** Read the head, confirm `expectedRevision`, apply the edits, compile, check, then store in a transaction that re-reads the head and the facts' version. No connection is held across a compile. There is no override argument: unknown arguments are stripped by validation.

| Error code | Means |
|---|---|
| `cv_facts_not_locked` | The owner hasn't locked the facts |
| `cv_revision_conflict` | The head moved. Carries `currentRevision` and `expectedRevision` |
| `cv_facts_changed` | The owner changed the facts during the compile. Nothing was stored: try again |
| `cv_edit_no_match`, `cv_edit_ambiguous` | An `oldText` matched 0 or several times. Carries `edit` (its index), `oldText` and `matches` |
| `cv_no_change` | The edits leave the source as it was |
| `cv_compile_failed` | Carries `diagnostics` |
| `cv_fact_violation` | A hard refusal. Carries the first 50 `violations`, each with `rule`, `token` and `context`, and `truncated`, the number left out |
| `cv_approval_required` | The text isn't approved, so no document was stored. Carries `variant`, `revision` and `approvalPath`. Only the owner can go on from here |
| `cv_approval_stale` | Only on the owner's paths. The head or the facts moved since the owner read it. Carries `currentRevision` |
| `cv_variant_not_found`, `cv_revision_not_found`, `cv_variant_exists`, `cv_variant_name_invalid` | As named. A new variant's name is letters, digits, `-`, `_` and `.`, and can't start with `application-` |
| `cv_render_busy` | Compiles run one at a time, with at most two calls waiting. A third is refused at once, and one that has waited 20 s for its turn gives up. Nothing was stored |
| `cv_source_too_large`, `cv_too_many_pages`, `cv_pdf_too_large`, `cv_render_timeout`, `cv_render_memory_exceeded`, `cv_render_failed` | The worker's limits. `cv_too_many_pages` also covers rendered text over its limit. `cv_pdf_too_large` is found only by a full render, since patches compile without a PDF |
| `cv_render_result_too_large` | The PDF and previews together pass 12 MiB, which is what the transport is sized for. A PDF at its 10 MiB limit with a normal preview fits, tested over the real transport |

**`generate_tailored_cv`.** The section 4 gap closes: it generates a source from the profile and patch, stores it as a revision of variant `application-<id>` (`origin: generate`) unless its head already holds that source, compiles, checks, and stores the PDF as a document linked to the application, replacing the previous one unless that was submitted. **A deliberate difference from old Nexus at `235138d`:** its `lib/cv/generate.ts` deletes the previous document even when another application still links it, which that application's CV loses. Here a document with other links stays, and only this link goes. The parity scenarios never link a generated document to a second application, so parity doesn't cover this. The response keeps the frozen fields: `message, documentId, originalName, size, applicationId, company, role`, and `warnings` when any, which hold only old Nexus's unmatched-ID messages. With locked facts and text the owner hasn't approved, it stores the revision, no document and no patch, and returns `cv_approval_required`, as the table above says. It needs no lock, as old Nexus had none: while unlocked it stores a stamped PDF and answers as old Nexus would, so a parity caller gets the same response. A hard refusal stores nothing. The parity differences change from "M1 gap" to "PDF bytes differ", since old Nexus used react-pdf.

**Reactive Resume** stays usable until the owner has moved real applications across. Removing the old `/resume` and `/tailor` routes is already part of cutover (section 10).

## 9b. M3 design: PWA and voice

**No WASM.** The handoff said Leptos islands. M1 chose SSR without WASM, and M3 keeps that: server-rendered pages, working forms, and one small hand-written `/assets/app.js` for what forms can't do. That saves a second build target and a large download on a phone, at the cost of the handoff's stack word. It's a recorded change, not an oversight.

**PWA.** `manifest.webmanifest`, icons and `sw.js`. The service worker caches only static assets and one offline page: **never** a page or JSON response, because they hold personal data. Installable on Android Chrome.

**Interactive dashboard.** The owner can append a note, change rating and triage fields, record lifecycle events (the one way to change status), add a contact and archive. Forms call the same tool functions as MCP through one internal entry point, so lifecycle rules and validation can't diverge. Every POST carries a CSRF token signed against the session. Old Nexus's lifecycle rule applies: mixed calls fail whole.

**CV workspace (`/cv`).** Variants at `/cv/variants/{variant}`, because `/cv/{variant}` couldn't tell a variant named `facts` from the facts page. `/cv` lists the revisions awaiting approval, and the top nav shows a small count while the facts are locked. A variant page has a draft or approved badge, the review items in plain language with the most serious first, the rendered diff with highlights on the changed lines, the source diff below, the stamped preview, an Approve button and the source editor, with revision list, diffs and revert. Besides those: `preview.png`, `preview.json`, `cv.pdf`, `approve` and `/api/cv/{variant}/head`. The facts page has lock and unlock, with the lock's list. Human edits pass the same hard rules, through the same code as `patch_cv`, and are approved by being saved there. Preview PNGs are cached in `<data>/cache/renders/`, which backups skip and anything may delete, emptied at start because a new binary may render or judge differently and the key can't see that. One file per render, not one per PNG.

**Live preview.** `GET /api/cv/{variant}/events` is an SSE stream of new revisions and approvals, fed by an in-process channel. An approval makes an open preview drop its stamp in place. `X-Accel-Buffering: no`, a heartbeat, at most 4 streams, ended at shutdown, and outside the owner pool's 60 s deadline. If the stream breaks, the page falls back to a slow poll. A facts change sends no event, so an open page keeps its old preview until reload, and the check and the cache are still right at the next render.

**Voice session.** OpenAI Realtime over WebRTC, directly from the browser. Push-to-talk is the default, with hands-free as a toggle: one connection path, not two systems.

1. `POST /api/voice/session`: owner session and CSRF. Nexus calls OpenAI with a key from `NEXUS_OPENAI_API_KEY_FILE` and returns a short-lived client secret plus a Nexus voice session id. The real key never reaches the browser. Without a key file, the feature is off and the page says so.
2. The browser sets up WebRTC with that secret.
3. The model's function calls arrive on the data channel. The browser sends each to `POST /api/voice/tool`, with the voice session id.
4. **The allowlist is enforced by the server, on that route.** The session's tool definitions come from it too.

| Allowed | Tools |
|---|---|
| CV | `get_cv_source`, `patch_cv` (origin forced to `voice`), `render_cv_pdf` (no save, `previewPages` forced to 0, and only its JSON summary reaches the model), `list_cv_revisions`, `revert_cv` |
| Read-only pipeline | `list_applications_filtered`, `get_application`, `list_application_events` |

**Gate: a voice session can't mark anything `applied`.** No allowed tool writes outside the CV tables. Every other tool, known or unknown, returns `{"error":{"code":"voice_tool_not_allowed"}}` without running. A test tries all tools with lifecycle arguments and compares a hash of every non-CV table before and after. Prompt injection from a job description can therefore reach only the CV tools, where what it writes stays a draft the owner reads.

**Voice can't approve.** It can patch, and the preview stays stamped. `voice_instructions.txt` tells the model it can't approve, that the owner approves on screen, and never to call a CV final unless `state` says `approved`. A CV tool call with no `variant` uses the session's variant, not the schema's `default`, which needn't be the page on screen. An explicit one is still honoured.

**Limits.** Client secret 60 s, 2 live sessions with the oldest evicted, 12 mints an hour including failures and reset by restart, tool body 1 MiB, session body 4 KiB, session length at most 3600 s. Settings: `NEXUS_REALTIME_VOICE`, `NEXUS_VOICE_SESSION_SECONDS`, and the hidden `--voice-test-base-url`, accepted only with `--insecure-local-only` and only as `http://127.0.0.1:<port>`. New codes: `voice_argument_not_allowed`, `voice_session_invalid`, `voice_disabled`, `voice_rate_limited`, `voice_upstream_failed`, `voice_bad_request`, `voice_failed`, `csrf_invalid`, and in the browser only `voice_invalid_arguments`, `voice_network_error` and `voice_http_<status>`.

**Headers.** "No microphone elsewhere" is `Permissions-Policy: microphone=()` on every page from the web crate. The auth crate's login pages carry no such header, and load no script. With no key, the variant page gets that strict form too: no OpenAI in `connect-src`, no `media-src`.

**A conflict on save** re-renders with the new head as the base, so saving again replaces it. That is a choice made after reading the notice, and history keeps both. `Serve(Box<ServeArgs>)` in `main.rs` is what clippy required once the struct grew.

**TLS for the call to OpenAI: `hyper-rustls` with `ring` and `webpki-roots`, not `reqwest`.** `aws-lc` is what `reqwest`'s `rustls` feature pulls, and its `rustls-no-provider` unifies across the workspace and makes every `reqwest::Client`, test clients included, panic. **`ring` contains C and assembly**, against the letter of "no C beyond SQLite". It needs no system library and cross-builds, and it stays because the alternatives are alpha or young: `rustls-rustcrypto` and `graviola`, rejected unbuilt.

**Cost and data.** Sessions have a server-enforced lifetime (15 minutes by default) and an idle cut-off in the page. Everything the allowed tools return goes to OpenAI: this includes job descriptions and event notes. The owner opts in by creating the key file.

**Sandbox.** The unit's `IPAddressDeny=any` blocks the call to OpenAI, and OpenAI's addresses aren't stable enough to list. `deploy/voice.conf` is an optional drop-in that lifts the filter, and using voice means accepting that. Nothing else in Nexus makes outbound calls.

**What can't be tested without the owner:** a real Realtime session (costs money and sends data to OpenAI), a phone's microphone permission, and Telegram through Hermes.

## 10. Open items

| Item | Blocks | Owner |
|---|---|---|
| Encrypted backup destination | Deployment | Christian |
| Review the 26 recovered document candidates and decide what to do about the 6 with none | Importer | Christian |
| Is the Neon project still used for anything? Live Nexus doesn't point at it | Local notes, cleanup | Christian |
| Live `tools/list`, to confirm the snapshot | Freezing the contract | Agent, with connector access |
| Does the board's Ethernet drop recur? Why did it drop on 2026-11-06? The board stays the chosen host; check `journalctl -b \| grep 'end0'` before cutover | Cutover | Christian, watch logs |
| Ingress choice | M1 gate | Test both, then decide |
| Do any workflows rely on old Nexus's `/resume` and `/tailor` routes, which call Reactive Resume? They disappear at M1 cutover. `resume-tailoring` and `job-application-tracking` were not available to QA | Cutover | Check those skills |
| Did the Typst/Reactive Resume memory update land? (Hindsight was down) | Nothing | Check later |
