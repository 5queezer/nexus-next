# Cutover runbook

From today's state (M1 code merged, nothing deployed, old Nexus live) to Nexus on the board. Do the steps in order. Placeholders: `<old-host>`, `<board>`, `<test-host>`, `<final-host>`, `<db-container>`, `<app-container>`, `<db-name>`. Commands marked **workstation**, **board** or **old host** say where they run. Fuller explanations are in [`deploy/README.md`](../deploy/README.md).

**Owner only:** every step that touches old Nexus, DNS, Cloudflare or Tailscale, and the steps needing a browser or a choice: the passphrase, the backup key, consent in Claude.ai and Hermes, the recovered-document review, the ingress choice and the off-site destination.

## What has never been run

Treat these as untested, and stop if one behaves differently from what is written here.

| Step | Status |
|---|---|
| Install under real systemd, `IPAddressDeny`, `PrivateUsers` (steps 2, 3) | Never run. The binary ran on the board once, by hand, with synthetic data. `IPAddressDeny` isn't enforced under `systemd-run --user`, so it has never been seen working |
| Any ingress, Claude.ai and Hermes against this server (steps 4, 12) | Never run. The Claude.ai callback host and the Tailscale Funnel client-address header are unverified |
| Freezing old Nexus and unfreezing it (step 7, rollback) | Never run on old Nexus. The SQL was tested on a local Postgres 17 |
| Final export (step 8) | `deploy/export/` was tested on 2026-11-06 against a local Postgres restored from the first dump, with a stubbed storage client: the rows, sequences and imported database were identical to the first export. It has not been run over ssh or against real GCS since being ported. The same blob code did run against GCS for the first export |
| Import on the board, with real data (step 9) | Never run. It ran on the workstation, with and without approvals. The real data set has never been measured on the board |
| Parity against a new baseline (step 10) | Run once on 2026-11-06, as written, on a re-export of the first dump. Never on real final data |
| Restore drill with the real key, the backup timer, off-site sync (steps 6, 14) | Never run. No off-site destination exists |
| Every rollback below | Never run |

## 1. Board prerequisites

**Board:**

```sh
journalctl -b | grep end0        # no link-down lines since the last reboot. See below
free -h                          # about 1.8 GiB available was the last reading
df -h /                          # the blobs and backups fit on the root filesystem
timedatectl | grep synchronized  # yes: token expiry depends on the clock
systemctl --version | head -1
```

The Ethernet link dropped for 22 minutes on 2026-11-06, cause unknown, taking the tunnel with it (SPEC section 8). The old host stayed up. If it has recurred, decide whether to go ahead before going further: **this is a go/no-go**, because no step below fixes it.

Check that nothing still needs old Nexus's `/resume` and `/tailor` routes (SPEC section 10), because they disappear at cutover.

## 2. Install

Follow `deploy/README.md` sections 1 to 3: build, install, calendar token, `nexus.env`, passphrase, public backup key. For now, set the test origin and the header for the ingress you will test first:

```sh
NEXUS_PUBLIC_BASE_URL=https://<test-host>
NEXUS_TRUSTED_IP_HEADER=CF-Connecting-IP   # Cloudflare. For Funnel, see step 4
```

Import the first export, which is enough to test with, then start (sections 4 and 5). Step 9 replaces this data, and everything authorized on it.

Reboot with `/home` locked. `systemctl is-active nexus` and `curl -fsS http://127.0.0.1:8080/healthz` must succeed with nothing unlocked.

## 3. Verify the sandbox under real systemd

**Board:**

```sh
PID=$(systemctl show -p MainPID --value nexus)

# PrivateUsers: the service sees only its own user and root, so few lines, none spanning all IDs.
sudo cat /proc/$PID/uid_map
stat -c '%U %a %n' /var/lib/nexus /var/lib/nexus/nexus.db   # nexus 700, nexus 600

# IPAddressDeny: was it applied at all? Without cgroup BPF in this kernel, systemd only warns.
systemctl show nexus -p IPAddressDeny -p IPAddressAllow
journalctl -u nexus -b | grep -iE 'bpf|ip firewall'         # must print nothing

# Enforced? Same settings, real system manager: the first must fail, the second must print ok.
sudo systemd-run --wait --pipe -p IPAddressDeny=any -p IPAddressAllow=localhost \
  curl -sS -m 5 -o /dev/null https://example.com; echo "exit $?"
sudo systemd-run --wait --pipe -p IPAddressDeny=any -p IPAddressAllow=localhost \
  curl -fsS -m 5 http://127.0.0.1:8080/healthz
```

If the filter isn't enforced, the service can still reach the network if compromised. Record that and decide: it doesn't stop the cutover by itself.

## 4. Ingress test: both options

Test Cloudflare and Funnel, on `<test-host>`, with the same checks. Only one can be live at a time, and each needs its own `NEXUS_PUBLIC_BASE_URL`, then `sudo systemctl restart nexus`.

**Set up, Cloudflare tunnel:** `cloudflared` on the board with ingress `http://127.0.0.1:8080`, and `NEXUS_TRUSTED_IP_HEADER=CF-Connecting-IP`. A tunnel is always proxied. Old Nexus stayed DNS-only because its notes say the proxy broke MCP.

**Set up, Tailscale Funnel:** `sudo tailscale funnel --bg 8080`. Base URL is the `*.ts.net` name. Which header carries the client address isn't confirmed: set `X-Forwarded-For`, then check the source address in check 3 below.

**Check 1, HTTP.** Workstation. Issue a token on the board first: `sudo -u nexus nexus issue-token --data-dir /var/lib/nexus --name ingress-test`, and put it in `TOKEN`.

```sh
BASE=https://<test-host>
curl -fsS $BASE/healthz
curl -fsS $BASE/.well-known/oauth-authorization-server | python3 -m json.tool | grep '"issuer"'  # exactly $BASE
```

**Check 2, buffering.** This server sends whole responses: a `POST` is one `application/json` body with `Content-Length`, and a `GET` is an empty `text/event-stream` that ends at once. It sets no `X-Accel-Buffering`. So nothing should depend on buffering, but that is the expectation this test exists to check.

```sh
H='accept: application/json, text/event-stream'
curl -sS -m 20 -D - -o /dev/null -w 'total %{time_total}s\n' -X POST $BASE/api/mcp -H "authorization: Bearer $TOKEN" \
  -H "$H" -H 'content-type: application/json' -H 'mcp-protocol-version: 2025-06-18' -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
# 200, a JSON content type, well under a second or two.
curl -sS -m 20 -D - -o /dev/null -w 'total %{time_total}s\n' $BASE/api/mcp -H "authorization: Bearer $TOKEN" -H 'accept: text/event-stream'
# 200, text/event-stream, and it must end at once rather than hang until -m 20.

python3 -I - > /tmp/big.json <<'PY'
import base64, json
body = b"%PDF-1.7\n" + b"synthetic ingress test\n" * 450000
print(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "upload_document_content",
      "arguments": {"filename": "ingress-test.pdf", "mimeType": "application/pdf", "contentBase64": base64.b64encode(body).decode()}}}))
PY
curl -sS -m 300 -w '\n%{http_code} %{time_total}s\n' -X POST $BASE/api/mcp -H "authorization: Bearer $TOKEN" \
  -H "$H" -H 'content-type: application/json' -H 'mcp-protocol-version: 2025-06-18' -d @/tmp/big.json | tail -c 200
# A near-limit 10 MiB upload through the ingress: 200, not a 413 or 524 from the ingress. Then delete the document.
rm /tmp/big.json
```

**Check 3, Claude.ai.** Add a connector for `https://<test-host>/api/mcp`, log in with the passphrase, approve, and run a tool: ask it to call `list_applications`. It must return data. **Board:**

```sh
sudo journalctl -u nexus --grep redirect_host_rejected -n 5   # the host Claude.ai really used, if refused
sudo journalctl -u nexus --grep 'event=' -n 20                # client_registered, consent_granted
```

If a host was refused, add it to `NEXUS_OAUTH_REDIRECT_HOSTS` (this replaces the default, so list everything), restart and retry. Write the host down: this is the fact the docs still lack. Then fail one login from another network: `login_failure` must show that network's address as `source=`, not `127.0.0.1`.

**Decide** which ingress to keep. If both fail check 3, stop: that is the M1 gate.

## 5. Approve recovered documents

**Owner.** 32 documents have no file. For 26 of them, candidates were recovered from Hermes data, with no stored hash to prove identity. Review `<first export>/recovered-candidates/REVIEW.txt`: it lists ID, match level, size, original name, linked applications and candidate paths.

Write the IDs you confirm to `$HOME/.local/share/nexus-export/approved-ids.txt`, outside any repo:

```text
# One document ID per line. # starts a comment.
123
456
```

Anything not listed imports as `blob_state = 'missing'`, and only its download fails. An ID with no candidate, or more than one matching equally, fails the import. Decide also about the 6 with no candidate (SPEC section 10).

## 6. Restore drill

Run `sudo systemctl start nexus-backup.service`, copy the newest file from `/var/lib/nexus-backups/` to the workstation, and do the drill in `deploy/README.md` section 6 with the real key. Don't start the freeze without having read a restored copy.

## 7. Freeze writes on old Nexus

**Owner.** From here until step 12, there is no writable Nexus. Pause anything that writes first: the Hermes Career Ops pipeline and any open Claude.ai session. The app container stays up, because step 8 needs its storage client.

**Old host:**

```sh
docker ps --format '{{.Names}}'     # Coolify names can change on a redeploy
Q() { docker exec -i <db-container> sh -c 'psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$1" -At' sh <db-name>; }
echo 'ALTER DATABASE "<db-name>" SET default_transaction_read_only = on;' | Q
echo 'SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid();' | Q
echo 'SHOW default_transaction_read_only;' | Q                                  # on
echo 'BEGIN; CREATE TABLE _freeze_check (); ROLLBACK;' | Q                      # must fail: read-only transaction
```

Writes through the app now return errors. An upload may still leave an unreferenced file in the bucket, which does no harm.

## 8. Final export

**Workstation**, from the repo:

```sh
umask 077
EXPORT=$HOME/.local/share/nexus-export/$(date -u +%Y%m%dT%H%M%SZ)
FIRST=$HOME/.local/share/nexus-export/20261106T001410Z
APPROVED=$HOME/.local/share/nexus-export/approved-ids.txt
install -d -m 700 "$EXPORT"
ssh <old-host> sh -s -- <db-container> <db-name> < deploy/export/dump.sh > "$EXPORT/nexuscrm.dump"
ssh <old-host> sh -s -- <db-container> <db-name> < deploy/export/rows.sh > "$EXPORT/rows.stream"
ssh <old-host> sh -s -- <app-container> <db-container> <db-name> < deploy/export/blobs.sh > "$EXPORT/blobs.stream"
python3 -I deploy/export/seal.py "$EXPORT" --recovered "$FIRST/recovered-candidates"
```

It prints counts only and must end with `VERIFY OK`. Compare with the first export: rows should have grown only by what you expect. **Documents with no file must still be 32**, or fewer if some were deleted: more means files have gone missing since, so stop and look.

Then a trial import, which also gives the counts the board must match:

```sh
cargo build --release -p nexus
DRY=$(mktemp -d)
target/release/nexus import --export "$EXPORT" --data-dir "$DRY/data" --approve-recovered "$APPROVED" | tee "$EXPORT.summary"
rm -rf "$DRY"
```

## 9. Import on the board

```sh
# workstation
ssh <board> 'sudo install -d -m 0700 -o "$USER" /var/tmp/nexus-export'
rsync -a "$EXPORT"/ <board>:/var/tmp/nexus-export/
scp "$APPROVED" <board>:/var/tmp/nexus-approved-ids.txt

# board
sudo systemctl stop nexus
systemctl is-active nexus                       # inactive
sudo chown -R nexus:nexus /var/tmp/nexus-export /var/tmp/nexus-approved-ids.txt
sudo -u nexus nexus import --export /var/tmp/nexus-export --data-dir /var/lib/nexus \
  --approve-recovered /var/tmp/nexus-approved-ids.txt --force
sudo rm -rf /var/tmp/nexus-export /var/tmp/nexus-approved-ids.txt
```

The printed counts must equal `$EXPORT.summary`. `--force` is needed because step 4 wrote OAuth grants, and it empties them.

If the import says the database **is in use**, a process still has it open: find it (`sudo fuser -v /var/lib/nexus/nexus.db`) and stop it. A WAL left by a crash doesn't cause this, because the importer replays it, and it doesn't matter here since the data is being replaced. The in-use check can't see a running server that has been idle for over 10 minutes, because its pool has closed every connection, which is why the `is-active` line above matters.

Leave the service stopped until step 10 passes.

## 10. Parity re-run

**Workstation.** The baseline comes from this export, not from the golden traces of 2026-11-06. **Those go stale from 2026-11-09**, because `pipeline_healthcheck` depends on the date (`parity/README.md`, "Time"), and the data has moved anyway. Re-record it on the day. Imports here have no approved documents, because the old baseline has none.

```sh
RUN=$HOME/.local/share/nexus-export/parity-final
install -d -m 700 "$RUN"
cp "$EXPORT/id-floors.json" parity/id-floors.json     # new IDs start above this export's. Don't commit it
export OLD_NEXUS_DIR=<checkout of nexus-crm at 235138d with deps> NEXUS_EXPORT_DIR="$EXPORT"
parity/env/pg.sh up
parity/runner-old/run.sh "$RUN/baseline"
parity/env/pg.sh down
parity/runner-rust.sh target/release/nexus "$EXPORT" "$RUN/rust"
python3 parity/test_compare.py
python3 parity/compare.py "$RUN/baseline" "$RUN/rust" --unordered contacts --unordered applications
git checkout parity/id-floors.json
```

Expect 3 documented calls (`generate_tailored_cv`) and 1 failure (`13-read-pagination-filters`, `sort: company`, explained in SPEC section 7). Anything else, including a failed `expect`, is a stop.

Then **board:** `sudo systemctl start nexus`, and check healthz.

## 11. Backup of the imported data

```sh
sudo systemctl start nexus-backup.service
```

Copy the file to the workstation and repeat the drill from step 6. This is the fallback from now on. Keep it.

## 12. Repoint clients

Both clients use OAuth, and **both must authorize again**, because the OAuth tables start empty.

1. **Owner:** point `<final-host>` at the chosen ingress. Set `NEXUS_PUBLIC_BASE_URL=https://<final-host>` in `/etc/nexus/nexus.env`, then `sudo systemctl restart nexus`. The issuer must be `https://<final-host>` (check 1 of step 4).
2. **Claude.ai:** remove the old connector, add `https://<final-host>/api/mcp`, log in, approve, run a read and a harmless write, such as a note on a test application.
3. **Hermes Career Ops:** point its MCP server URL at the same address, restart it and authorize. On a headless host that means pasting the callback by hand. `oauth.redirect_host` stays `127.0.0.1` or `localhost`. It must list 35 tools. Remove any stored token or registration for the old server first. A stale stored override has broken Hermes OAuth before.
4. **API token,** only if something uses one: `sudo -u nexus nexus issue-token --data-dir /var/lib/nexus --name <name>`.
5. **Board:** `sudo -u nexus nexus grants list --data-dir /var/lib/nexus` shows exactly those two grants. Resume the Hermes pipeline.

Above 1 MiB, `download_document_content` now returns content inline, where old Nexus returned a signed URL (SPEC section 7). Watch that the first large download works in each client.

## 13. Calendar subscription

The feed is `https://<final-host>/calendar.ics?token=<value of /etc/nexus/calendar-token>`. Remove the old subscription and add this one. Google Calendar fetches from its own servers, so it needs the public ingress; ICSx5 works from the device. The URL is a secret: don't paste it into chats or logs.

## 14. Backup timer and off-site

```sh
systemctl list-timers nexus-backup.timer       # next run listed
systemctl status nexus-backup.service --no-pager
```

**Owner:** choose the off-site destination and add the drop-in from `deploy/README.md` section 6, or accept the gap in writing. Until then, backups die with the board. The next day, check that a new file appeared.

## 15. Old Nexus retention

Keep it stopped but restorable for **30 days**.

1. Stop the app through Coolify. Don't delete it, or its Postgres volume. The database stays read-only.
2. Keep the GCS bucket as it is. Before decommissioning, review `_nexus-backups/pre-deploy/` (62 snapshots, 53 objects, at the 2026-11-06 count) and the 14 unreferenced root objects, for retention and for secrets. They are snapshots of a database with credential tables, so don't assume they're clean.
3. Update `CLAUDE.local.md` in the old `nexus-crm` checkout. It still describes the old host as live, with its container names, production SQL and OAuth cleanup commands, and says the domain must stay DNS-only. That advice was about the old host. Mark it historical, and delete it when old Nexus goes.
4. After the 30 days, and only with a good backup and the owner's go-ahead: delete the app, database and bucket, revoke the service-account key, and delete `$EXPORT`, the first export and the parity traces, all of which hold personal data. Settle the Neon question (SPEC section 10).

## Rollback

Old Nexus is the source of truth until step 12. **Nothing syncs back afterwards.**

| If it fails at | Do this | Lost |
|---|---|---|
| Steps 1-6 | Stop `nexus`. Old Nexus was never touched | Nothing |
| Steps 7-11 | Unfreeze (below), restart the app through Coolify, resume Hermes. Stop `nexus` on the board. Redo from step 7 on the next attempt | Nothing |
| Step 12, no client has written yet | As above, and point the host and clients back. Old OAuth state still exists; if auth fails, use the cleanup in `CLAUDE.local.md` | Nothing |
| After clients have written to the new server | Decide first. Going back loses those writes unless re-entered by hand: list them with `list_application_activity` while the new server is up. Then as above | Anything not re-entered |
| A bad upgrade later | `deploy/README.md` section 8 | Writes since the backup, if restoring |
| A bad restore | Stop the service and delete any `nexus.db-wal` and `nexus.db-shm`, which belong to the restored copy. `nexus.db.before-restore` is the replaced database, with its `-wal` and `-shm` if it had them. Rename them all back without `.before-restore`, and start | Nothing |
| Within 30 days, new server unusable | Unfreeze old Nexus as above and repoint | All writes since step 12 |
| After old Nexus is deleted | Restore from a backup (`deploy/README.md` section 6) | Writes since that backup |

To unfreeze, on the **old host**. The session needs `SET` first, because the frozen database refuses `ALTER DATABASE` otherwise:

```sh
Q() { docker exec -i <db-container> sh -c 'psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$1" -At' sh <db-name>; }
echo 'SET default_transaction_read_only = off; ALTER DATABASE "<db-name>" RESET default_transaction_read_only;' | Q
```
