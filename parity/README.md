# Parity baseline

Golden request/response traces from old Nexus (`nexus-crm` at `235138d`), for comparison with the Rust server. Scenarios and tools are committed here. Traces hold personal data, so they stay outside the repo.

| Path | Purpose |
|---|---|
| `scenarios/*.json` | Ordered tool calls. Personal values are discovered at runtime; only integer IDs and invented strings are committed. |
| `runner-mcp/run.mjs` | Streamable HTTP MCP client that runs scenarios against any URL. No dependencies. |
| `runner-old/` | Serves the real old route handlers on loopback, then drives them with `run.mjs`. |
| `env/pg.sh` | Throwaway Postgres 17 and uploads dir, restored from the export. |
| `compare.py` | Compares two trace directories. Its docstring is the complete list of normalizations. |
| `id-floors.json` | Highest IDs in the export. Higher IDs were created during a run. |

## Run the old baseline

```bash
export OLD_NEXUS_DIR=/path/to/nexus-crm-at-235138d   # with npm dependencies installed
parity/env/pg.sh up                                  # rebuilds everything from the dump; prints row counts
parity/runner-old/run.sh ~/.local/share/nexus-export/parity-golden/baseline
parity/env/pg.sh down                                # stops Postgres and deletes the working data
```

`pg.sh up` is also the restart. Its data is on tmpfs, so nothing survives `down` or a reboot, and nothing is lost either: the dump is the source. Defaults: port `55417` (`PARITY_PG_PORT`), export `~/.local/share/nexus-export/20261106T001410Z` (`NEXUS_EXPORT_DIR`), working dir `$XDG_RUNTIME_DIR/nexus-parity` (`PARITY_WORK`).

Expected row counts: Application 190, ApplicationEvent 205, Contact 36, Document 81, `_ApplicationToDocument` 56, ApplicationSubmission 1.

`runner-old` stays on loopback: GCS variables are unset, so storage uses the local uploads dir, `DATABASE_URL` must be loopback, and `fetch` to any other host throws. The harness does not modify old code. It replaces one thing, the `require()` in `lib/db/index.ts` that can't load TypeScript under vitest, with the same `PrismaAdapter`.

## Run against the Rust server

```bash
node parity/runner-mcp/run.mjs \
  --server-url http://127.0.0.1:PORT/api/mcp \
  --token "$FULL_TOKEN" \
  --token-tools-only "$TOOLS_ONLY_TOKEN" \
  --reset-cmd 'path/to/restore-rust-db-from-export' \
  --out /private/path/rust-run

python3 parity/compare.py ~/.local/share/nexus-export/parity-golden/baseline /private/path/rust-run
```

The server under test needs:

- **The imported export, restored by `--reset-cmd`.** It runs before every scenario with `resetDb: true`, and it must restore the blobs too. The 32 documents with no file must stay missing.
- **Two tokens.** `--token` has `mcp:tools` and `mcp:submissions`. `--token-tools-only` has only `mcp:tools`, and old Nexus only enforces this for OAuth tokens: API tokens always carry both scopes.
- **The owner's real email as the actor.** Event request hashes cover the actor, and new events echo it. `env/pg.sh` takes it from the most common `actor` on real MCP events.
- **The same clock era.** See "Time" below.

`--only <substring>` runs matching scenarios, which helps while porting.

The exit code is 0, or 3 if any `expect` failed. Anything else is a crash.

## Scenario format

```json
{
  "name": "40-events",
  "resetDb": true,
  "calls": [
    { "tool": "create_application", "args": { "company": "Parity Labs", "role": "R" }, "save": { "app": "." } },
    { "tool": "get_application", "args": { "id": "$app.id" }, "expect": { "isError": false, "paths": { "remote": false } } }
  ]
}
```

| Key | Meaning |
|---|---|
| `tool` | Tool name, or `$tools/list` |
| `args` | Arguments. `"$name.path"` is a saved value, `"x${name.path}"` interpolates, `{"$repeat":["x",101],"prefix":"…"}` builds a long string. |
| `auth` | `full` (default), `tools-only`, `invalid`, `none` |
| `save` | `{ "name": "path" }` into the parsed response. Paths use dots and indices; `.` is the whole value; `?key` keeps array items with a truthy key. |
| `expect` | `isError`, `errorCode` (`error.code`), `text`, `httpStatus`, `paths` (`"$absent"` for missing). Failures are recorded in the trace. |
| `compare` | `isError`, `isError-set` or `batch-results`, explained in `compare.py`. |
| `allowDiff` | A reason. Differences are listed as documented and don't fail the run. |
| `forEach` + `bind` | Repeats the call for each item of a saved list. |
| `parallel` | A list of calls sent concurrently. `compare.py` sorts each group before comparing. |

## Trace format

One JSON file per scenario, the same from any MCP client:

```json
{
  "format": 1, "scenario": "40-events", "startedAt": "…", "finishedAt": "…",
  "calls": [
    { "tool": "…", "auth": "full", "args": {},
      "isError": false, "contentCount": 1, "json": {}, "structured": "same-as-text" }
  ]
}
```

- A response is `json` if its first text block parses, otherwise `text`. JSON key order and whitespace aren't compared.
- `structured` is `none`, `same-as-text`, or the `structuredContent` value when it differs.
- `rpcError` replaces the result for a JSON-RPC error, and `httpStatus` for an HTTP failure.
- `contentBase64` is stored as `{sha256, base64Length}`, in arguments and in responses.

## Comparing

`compare.py` prints a per-tool table and precise JSON paths, with sizes and types instead of values. Add `--show-values` to see values, which are personal data.

Use `--drop-key` for a removal that has been decided on. For example, every old response carries `userId`, and applications and events carry `isDemo`, `demoKey` and `demoWorkspaceId`, though section 5 of the spec drops those columns. The ignored keys are printed with the results.

## Time

`pipeline_healthcheck` reports `overdue_follow_up` by comparing `followUpAt` with the current time, so its output depends on when it runs. Two active applications in the export have follow-ups on 2026-11-09, so traces recorded before that date stop matching after it. Re-record the baseline on the day of comparison: the dump makes it reproducible.

Old Nexus was not frozen in time, because Prisma generates `updatedAt` inside its engine, out of reach of a JavaScript fake clock. Generated timestamps are normalized instead. So timestamps generated by a run are only checked as "generated during the run", and their order isn't checked; `expect` on `updatedAt` covers the dry-run cases.

## What the baseline doesn't cover

- Object storage: with GCS unset, files over 1 MiB come back inline, where production returns a signed URL.
- Concurrent writes beyond the `parallel` groups, and anything over a real network.
- The OAuth endpoints (`authorize`, `token`, `register`): only bearer checking on `/api/mcp` is exercised.
- `event_metadata_too_large`: the keys each recordable event type allows can't reach 32,000 bytes.
