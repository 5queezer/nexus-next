# Nexus rewrite: spec (M1 detail, M2+ outline)

Status: draft, 2026-11-06. No code yet.
Baseline: `nexus-crm` at `235138d`, the commit in the running production image's tag on 2026-11-06. Sources marked **[handoff]** are not yet checked against the code or the Nexus skills.

## 1. Goal

Replace the Next.js Nexus with one Rust binary on a ROCK 4 SE: an MCP tool server for agents, with a read-only web dashboard for humans. The live system stays the system of record until the M1 gate passes.

## 2. Decisions

| Decision | Choice | Consequence |
|---|---|---|
| CV rendering | No renderer in M1. Typst arrives in M2 | `CvProfile` and `CvPatch` are empty in production and no stored CV came from `generate_tailored_cv`. See section 4 for its M1 behaviour. |
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
2. **Golden test.** CI compares the server's `tools/list` with the snapshot. Any difference fails the build. M2 tools are additive and go in a separate allowlist.
3. **Keep the quirks.** Limits differ per tool, and clients may depend on that:
   - `source` has `maxLength: 100` on `upsert_application_by_job_url` and `update_document_metadata`, but no limit in the schema on `create_application` or `update_application`.
   - `rating` is an integer on create and upsert, but a number on update.
   - `update_application` advertises lifecycle fields, then rejects them.
4. **Lifecycle rule.** `update_application` with any of `status`, `appliedAt`, `lastContact`, `followUpAt` or `currentStage` returns `{"error":{"code":"lifecycle_event_required","fields":[...]}}` as a tool error, and nothing is written.
5. **Confirm against production.** The snapshot was generated from the deployed commit, not read from the running server. Compare it with a live `tools/list` once before freezing.
6. **`generate_tailored_cv` in M1.** The tool stays listed with its frozen schema. With no profile it returns old Nexus's exact tool error, `No CV profile found. Use upsert_cv_profile first.`. `get_cv_profile` and `upsert_cv_profile` work normally. If a profile exists, the tool returns a clear tool error saying rendering arrives in M2, and writes nothing. This is the one documented gap in the frozen behaviour, and it can't affect current data.

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

**Parity result, 2026-11-06.** Golden scenarios of 1,809 calls through the real `nexus serve` binary, compared with old Nexus at `235138d` restored locally: 1,805 pass, 3 documented (the `generate_tailored_cv` gap), 1 fail (`sort: company`). That one is an artifact of the baseline: it ran under glibc `en_US` collation, while production compares bytewise (`'Z' < 'a'` is true there), as Rust does. Contact and linked-application order is compared unordered, because old Nexus returned Postgres heap order, which no client can rely on. About 18,000 further fuzzed calls matched apart from explained differences. The baseline goes stale for time-dependent `pipeline_healthcheck` checks from 2026-11-09, so re-record it after that.

**Cutover:** stop writes to old Nexus, re-export, import, re-run parity, then repoint clients. Keep old Nexus stopped but restorable for 30 days. The runbook is [`CUTOVER.md`](CUTOVER.md).

## 8. Deployment and open risks

**Board, read on 2026-11-06.** ROCK 4 SE, 6 cores, aarch64, Armbian 26.11.0-trunk (Debian 13), kernel 6.18. Armbian trunk is a development image, so expect kernel changes. RAM is 3.8 GiB with about 1.8 GiB available, alongside Coolify, Forgejo, CLIProxy and Joplin. NVMe root has 66 GiB free. `/home` is on a LUKS partition that stays locked after reboot, and was locked when read.

**Storage.** SQLite and blobs live on the root filesystem under `/var/lib/nexus`, never in `/home`, so the service survives an unattended reboot.

**RAM.** Target 30-60 MB at idle **[handoff]**. Measured on the board on 2026-11-06 at `0d5b5d8` with synthetic fixtures: 6.8 MB after start, 12.7 MB after 500 requests including 200 MCP calls, tool calls about 10 ms, binary 12.8 MB. With the real data set on x86-64, an earlier measurement before MCP was merged: 7.5 MiB after start, 29 MiB peak under 16 parallel clients. At `ab910ed` with `synchronous=FULL`, also on the board with synthetic fixtures: 13.2 MB peak, and 100 sequential `create_application` calls took 46.6 ms each including `curl` start-up. The real data set hasn't been measured on the board. Typst's compile peak and font loading are the main unknowns for M2.

**Backup.** `nexus backup` writes one age-encrypted file from a `VACUUM INTO` snapshot and the blobs it references, on a systemd timer (`deploy/README.md` section 6). The off-site destination is undecided.

**Durability.** `Db::open` sets `synchronous=FULL`, because an acknowledged submission must survive power loss: under `NORMAL`, a reboot can silently undo writes an agent already reported saved. QA's cost, on an x86-64 desktop NVMe (btrfs, 2 pinned cores) through real MCP, `NORMAL` then `FULL`: one agent p50 0.59 then 1.60 ms, p99 6.15 then 17.84 ms, 1,324 then 341 writes a second. Eight agents flat out: p50 1.65 then 11.72 ms, p99 34.50 then 333.20 ms, 2,430 then 348 writes a second, slowest single write 5.4 s against a 10 s busy timeout. Reads are unchanged. The board has not been measured, and real load is a tiny fraction of this. FULL does not cover a drive that lies about flushes. Real power loss has not been tested. `journal_size_limit` is 64 MiB: a checkpoint truncates a WAL grown past that once it restarts. The WAL can still grow beyond it under sustained load.

**Ingress: must be tested, not assumed.** Claude.ai needs a public URL.
- A Cloudflare tunnel keeps a stable custom domain, but local notes say the Cloudflare proxy breaks MCP streaming unless buffering is disabled.
- Tailscale Funnel avoids Cloudflare, but the hostname becomes `*.ts.net`, so clients change URL once.
- M1 gate: a full Claude.ai OAuth and tool call through whichever one is chosen.

**Limits.** Each kind of caller has its own pool of request slots: public (login, OAuth, calendar: 8 in flight, 2 s wait, 10 s deadline), owner (dashboard, JSON API: 8, 10 s, 60 s) and mcp (8, 10 s, 60 s, or 300 s for a large body). A full pool gives a 503. On the MCP route, a body over 1 MiB or of unknown length, and any `download_document_content`, also needs one of 2 large slots, because one 10 MiB document costs about 85 MB in the transport. At most 256 connections. Details in `deploy/README.md` section 7.

**Calendar.** Calendar apps can't send bearer tokens, so `/calendar.ics` needs a secret URL token. Google Calendar fetches subscriptions from its own servers and can't reach a tailnet-only URL. Use an on-device subscriber such as ICSx5, or publish the URL through the public ingress.

**Availability.** The board's logs for 2026-11-06 show a reboot at 00:15 (cause unknown), then the Ethernet link down from 00:26:39 to 00:49:07 (cause unknown) with the board still running. LAN, Tailscale and the Cloudflare tunnel all failed together, taking Forgejo and CLIProxy with them. The current host, `hetzner-hel`, stayed up throughout. Nexus would inherit this failure mode on the board. Agents need a clear error when Nexus is down, and no parallel tracker.

## 9. Milestones

| Milestone | Scope | Gate |
|---|---|---|
| M1 | Core, 35 tools (`generate_tailored_cv` per section 4), OAuth and tokens, REST, calendar, importer, read-only dashboard | Golden contract test, parity on real data, Claude.ai through real ingress, RAM measured on board |
| M2 | Typst renderer behind `generate_tailored_cv`; source and revisions: `get_cv_source`, `patch_cv`, `render_cv_pdf`, `list_cv_revisions`, `revert_cv`; Telegram dictation | Voice edits can't change hard facts |
| M3 | Interactive PWA, push-to-talk, realtime session with a narrow toolset | Session can't mark anything `applied` |
| M4 | ratatui TUI | |
| M5 | Native wrap, only if the PWA proves painful | |

M2 note: `CvProfile.experience` mixes hard facts (dates, company) with wording (bullets) in one JSON column. The fact/wording split the integrity rule requires is a real schema change, not a rename.

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
