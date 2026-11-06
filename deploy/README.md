# Deploying Nexus on the ROCK 4 SE

Target: Armbian (Debian 13), aarch64, systemd. Data lives on the root filesystem in `/var/lib/nexus`, never in `/home`, so the service starts unattended after a reboot while `/home` is still locked.

Files here:

| File | Install to |
|---|---|
| `nexus.service` | `/etc/systemd/system/` |
| `nexus-backup.service`, `nexus-backup.timer` | `/etc/systemd/system/` |
| `nexus.sysusers` | `/etc/sysusers.d/nexus.conf` |

## What this build serves

| Path | Who |
|---|---|
| `/healthz` | Open |
| `/calendar.ics` | The secret token in its URL. Not served without a token file |
| `/`, `/queue`, `/applications/*`, `/api/*` | The owner, logged in at `/login` |
| `/.well-known/oauth-*`, `/api/mcp/{register,authorize,token}` | OAuth 2.1 for MCP clients |
| `/api/mcp`, `/mcp` | MCP tools: an OAuth access token or the API token |

`nexus serve` refuses to start without `NEXUS_PUBLIC_BASE_URL` and an owner passphrase (section 3). `--insecure-local-only` drops the login and the OAuth routes and forces a loopback bind. It exists for local parity runs and restore drills, and answers only to `localhost`, `127.0.0.1` and the bind address, so a web page can't reach the data through DNS rebinding or a cross-site POST. Never add it to the unit on the board.

## 1. Build (on the workstation)

```sh
CROSS_CONTAINER_ENGINE=podman cross build --release -p nexus --target aarch64-unknown-linux-musl
file target/aarch64-unknown-linux-musl/release/nexus   # ELF 64-bit ARM aarch64, statically linked
```

## 2. Install (on the board)

```sh
scp target/aarch64-unknown-linux-musl/release/nexus deploy/nexus.service deploy/nexus-backup.service \
    deploy/nexus-backup.timer deploy/nexus.sysusers <board>:/tmp/

# on the board
sudo install -m 0755 /tmp/nexus /usr/local/bin/nexus
sudo install -m 0644 /tmp/nexus.sysusers /etc/sysusers.d/nexus.conf
sudo systemd-sysusers
sudo install -m 0644 /tmp/nexus.service /tmp/nexus-backup.service /tmp/nexus-backup.timer /etc/systemd/system/
```

Keep the previous binary for rollback: `sudo cp -p /usr/local/bin/nexus /usr/local/bin/nexus.prev` before every upgrade.

## 3. Secrets

Secrets are files owned by root in `/etc/nexus`. systemd copies them into a per-service credentials directory (`LoadCredential`), so the service user never needs access to `/etc/nexus` and no secret is ever on a command line.

```sh
sudo install -d -m 0750 /etc/nexus

# Calendar feed token. The feed URL is https://<host>/calendar.ics?token=<this value>
openssl rand -hex 32 | sudo install -m 0600 /dev/stdin /etc/nexus/calendar-token
```

Settings go in `/etc/nexus/nexus.env`:

```sh
sudo install -m 0640 /dev/stdin /etc/nexus/nexus.env <<'ENV'
# Required. The OAuth issuer, the same setting as --base-url. Must be the origin clients really use.
NEXUS_PUBLIC_BASE_URL=https://nexus.example.net
# Required with an https base URL: see below.
NEXUS_TRUSTED_IP_HEADER=CF-Connecting-IP
ENV
```

| Variable | Meaning |
|---|---|
| `NEXUS_PUBLIC_BASE_URL` | Required. An origin only: https, no path or query |
| `NEXUS_OWNER_ID` | Only for a database no import wrote. `nexus import` records the owner, and that value wins. MCP responses carry it as `userId` |
| `NEXUS_OWNER_EMAIL` | Only when the import found no MCP events to read it from. New events record it as `actor`, and their request hashes cover it |
| `NEXUS_TRUSTED_IP_HEADER` | The header your ingress sets to the client's address. Required with an https base URL: serve refuses to start without it. `none` says clients reach this server directly |
| `NEXUS_TRUSTED_PROXY_PEERS` | Comma-separated peers allowed to set that header. Default: loopback |
| `NEXUS_OAUTH_REDIRECT_HOSTS` | Comma-separated hosts an OAuth client may be redirected to. Default: `claude.ai,claude.com`. Replaces the default, so list everything you need. See below |
| `NEXUS_OWNER_PASSPHRASE_HASH_FILE` | Where the hash lives. Default: `<data dir>/owner-passphrase.argon2` |
| `NEXUS_OWNER_PASSPHRASE_HASH` | The hash itself, instead of a file. Overrides the file, and `set-passphrase` refuses to run while it is set |

### Trusted IP header: a required step behind a tunnel

Rate limits and login backoff key on the client's address. Behind `cloudflared` or Funnel every connection comes from loopback, so without this setting all of the internet is one client. One attacker request an hour then holds the login at its one-hour backoff, and a loop on the token endpoint gets real clients refused. So with an https base URL, serve refuses to start until this says either a header or `none`.

- **Cloudflare tunnel:** `NEXUS_TRUSTED_IP_HEADER=CF-Connecting-IP`.
- **Tailscale Funnel:** not confirmed yet. Check what Funnel really sends before relying on it; `X-Forwarded-For` works if Funnel appends the client address, because the last entry is the one used.

The header is only believed on connections from `NEXUS_TRUSTED_PROXY_PEERS`, so keep the bind on loopback and let nothing else on the board reach port 8080 with forged headers. Verify it after the first public request: failed-login log lines must show `source=<your real address>`, not `source=127.0.0.1`.

### OAuth redirect hosts

Registration is open, because Claude.ai registers itself. Without a limit, anyone could register a client that sends codes to their own site and then trick the owner into one click on "Allow". So a redirect URI must be `https` to a listed host, matched exactly with no suffixes or subdomains, or a loopback address (`localhost`, `127.0.0.1`, http or https, any port), which is always allowed. Clients already registered are held to the list at authorize too.

- **Claude.ai:** its real callback host is **not verified yet**. The default is a best guess. If connecting fails, look in the log for the host it really used, then set the variable to include it and restart:

  ```sh
  sudo journalctl -u nexus --grep redirect_host_rejected -n 5
  # event="redirect_host_rejected" host="what.it.used" source=...
  ```

- **Hermes:** nothing to set in its usual setup. Its MCP OAuth registers `http://127.0.0.1:<port>/callback`, which is loopback, including when you paste the callback by hand on a headless host. It needs the list only if its `oauth.redirect_uri` is set to an https proxy, or if a stored registration from before holds an https callback: then add that host. `oauth.redirect_host` must stay `127.0.0.1` or `localhost`. `http://[::1]` is refused, as it always has been.

Anyone can still register with an allowed host, such as loopback, and 30 of those in an hour use up the global registration allowance. That gains them no access, but real clients then get a 429 until the hour is up. `sudo systemctl restart nexus` clears it, which matters at cutover when clients must register again.

The path isn't limited, only the host, so an open redirect on a listed host could still leak a code. The attacker would also need the owner's click, and Claude's hosts are the only ones listed by default.

### Owner passphrase

Set it as the service user, after installing the binary, so the data directory and hash file belong to `nexus`. Never as root: the service could not read the result. It is read from a no-echo prompt, or one line of stdin, never from the command line.

```sh
sudo install -d -m 0700 -o nexus -g nexus /var/lib/nexus
sudo -u nexus /usr/local/bin/nexus set-passphrase --data-dir /var/lib/nexus
```

It needs at least 16 characters and can't be one short pattern repeated, and this login is on the public internet, so use a long one from the password manager. Only an argon2id hash is stored, at `/var/lib/nexus/owner-passphrase.argon2`, mode 600.

**Changing it** runs the same command, then `sudo systemctl restart nexus`. It revokes every OAuth grant and the API token at once and logs that, so Claude.ai and Hermes must each authorize again and any API token must be reissued. A running server keeps accepting the old passphrase until the restart, so do not skip it. The restart also ends all login sessions; login sessions live in memory, so any restart means logging in again.

**Locked out of the login?** Attempts are limited globally, so someone guessing from many addresses can keep the login closed. A browser that has logged in before has its own allowance and is unaffected, but a new phone or cleared cookies gets `Too many attempts`, for up to 5 minutes after the last guess. `sudo systemctl restart nexus` clears the limits at once. It also ends every login session and gives a guesser a fresh allowance, so look at `login_failure` in the log first.

Backups do not contain the hash. After restoring onto a new board, run `set-passphrase` again.

### API token

```sh
sudo -u nexus /usr/local/bin/nexus issue-token --data-dir /var/lib/nexus --name hermes
```

It prints the token once, on stdout. There is one token at a time, so this revokes any earlier one. Only its hash is stored.

### Backup key

Backups are encrypted with [age](https://age-encryption.org) to public keys only. Generate the key pair on the workstation, never on the board:

```sh
age-keygen -o nexus-backup.key           # prints "Public key: age1..."
```

Store `nexus-backup.key` in the password manager (and a second copy somewhere that survives losing both the board and the workstation). Put only the public key on the board; extra recipients (a spare key) go on extra lines:

```sh
echo 'age1...' | sudo install -m 0600 /dev/stdin /etc/nexus/backup-recipient
```

A backup nobody can decrypt is worthless: do the restore drill below before trusting the timer.

## 4. First import

Run the import before the first start, as the service user, from a directory outside `/home`:

```sh
sudo install -d -m 0700 -o nexus -g nexus /var/tmp/nexus-export
sudo cp -r <export dir>/. /var/tmp/nexus-export/ && sudo chown -R nexus:nexus /var/tmp/nexus-export
sudo -u nexus /usr/local/bin/nexus import --export /var/tmp/nexus-export --data-dir /var/lib/nexus
sudo rm -rf /var/tmp/nexus-export    # it is private data
```

The import prints counts and fails on any count, hash or foreign-key mismatch. Re-running it is safe; `--force` replaces a database holding rows the import did not write. Stop the server first: even with `--force`, an import refuses a database another process has open. A WAL left by a crash is replayed rather than refused, so its rows count as live data without `--force`.

The passphrase hash is a separate file, so setting it first is fine and an import never touches it. OAuth grants and the API token do live in the database: once any exist, a re-import needs `--force` and empties them. So issue the API token after the final import, and expect clients to authorize again after one.

## 5. Start

The passphrase and `nexus.env` from section 3 must be in place, or the service exits with an error saying which is missing.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now nexus.service nexus-backup.timer
curl -fsS http://127.0.0.1:8080/healthz        # ok
sudo journalctl -u nexus -n 20 --no-pager
systemctl list-timers nexus-backup.timer
```

Request logs carry method, route template and status only. Query strings, headers and bodies are never logged. `NEXUS_LOG_LEVEL` (default `info`) only changes verbosity; SQL text stays out at every level.

Security events are separate lines with an `event=` field, and carry no secrets or tokens. The only client-supplied text is a consent's name and redirect origin, and a refused redirect host: names lose their control and format characters and are cut to 64 characters, and both are quoted. Follow them with `sudo journalctl -u nexus --grep 'event='`:

| Event | Level | Means |
|---|---|---|
| `login_failure`, `login_lockout` | warn | A wrong passphrase; an attempt refused by backoff. `source` is the client address |
| `login_success` | info | The owner logged in. Unexpected ones mean the passphrase leaked |
| `client_registered`, `consent_granted` | info | A new OAuth client; the owner approved one, with its name and where it returns to |
| `redirect_host_rejected` | warn | A registration or authorization named a host outside `NEXUS_OAUTH_REDIRECT_HOSTS` (section 3) |
| `refresh_grace_used` | warn | A rotated refresh token came back within its 30 s retry window. Normal after a lost response. If it is frequent for one client, look closer |
| `token_replay_detected`, `grants_revoked` | warn | A used code or refresh token came back, so its grant was revoked. A client's next call fails until it authorizes again. Investigate, because it can mean a leaked token |
| `pkce_failure`, `rate_limited` | warn | A wrong PKCE verifier burned a code; a limit refused a request |
| `overloaded` | warn | A request got a 503 because its pool stayed full (section 7). `pool` says which |
| `connection_limit`, `accept_failed` | warn | New connections are being closed at the limit (logged at most once a minute); accepting failed, usually out of file descriptors |

To see what has been authorized, or to end one authorization without touching the others:

```sh
sudo -u nexus /usr/local/bin/nexus grants list --data-dir /var/lib/nexus
sudo -u nexus /usr/local/bin/nexus grants revoke <id> --data-dir /var/lib/nexus
```

The ID is the same as `grant=` in the log. A revoke takes effect at once, with no restart. The client stays registered and must authorize again. Check this list after any `consent_granted` you don't recognise.

Reboot test, with `/home` left locked: `sudo reboot`, wait, then `systemctl is-active nexus` and the healthz call above must succeed without anyone unlocking anything.

## 6. Backups

`nexus-backup.timer` runs at 03:30 UTC (plus up to 10 minutes of jitter) and writes `/var/lib/nexus-backups/nexus-<UTC timestamp>.tar.age`, then deletes all but the newest 14. Each file is one age-encrypted tar of a consistent `VACUUM INTO` snapshot plus every blob the snapshot references. Before packaging, the copy passes `integrity_check` and `foreign_key_check`, and every referenced blob must exist and match its hash; otherwise the run fails and leaves no file. It is safe while the server runs; a document deleted during a run can fail it this way, and the next run is fine.

The plaintext snapshot is staged in a mode 0700 `.backup-*` directory under `/var/lib/nexus`, which holds the same data anyway and is never synced. A kill leaves it there, and the next backup deletes it. It also deletes any older `.backup-*` left in the backup directory. Only one backup of a data directory runs at a time.

Run one now: `sudo systemctl start nexus-backup.service`. Run one before every upgrade.

### Off-site sync

The destination is not decided. Copy only the finished files, named `nexus-*.tar.age`: they are encrypted, and anything else in the directory is not meant to leave it. Add a drop-in with the hook once there is somewhere to send them:

```sh
sudo systemctl edit nexus-backup.service
```

```ini
[Service]
# The backup unit has no network access by default; a sync tool needs it.
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
# Runs only after a successful backup. Example with rsync over ssh (key readable by the nexus user):
ExecStartPost=/usr/bin/rsync -a --ignore-existing --include=nexus-*.tar.age --exclude=* -e "ssh -i /etc/nexus/offsite-key" /var/lib/nexus-backups/ backup@host:nexus/
```

A failing hook marks the unit failed, which shows in `systemctl --failed`.

### Restore drill (workstation or board)

```sh
nexus restore --from nexus-20261107T033000Z.tar.age --identity nexus-backup.key --data-dir /tmp/nexus-drill
nexus serve --data-dir /tmp/nexus-drill --bind 127.0.0.1:8099 --insecure-local-only   # look at it, then Ctrl-C
```

Restore verifies everything before touching the target: decryption (a wrong key, truncation or a changed byte installs nothing), `integrity_check`, `foreign_key_check`, and every blob against its hash. It refuses to overwrite an existing `nexus.db` without `--force`; with `--force` the old file is kept as `nexus.db.before-restore`, with its `-wal` and `-shm` if it had them, all mode 600. After a crash the WAL holds committed transactions, and SQLite replays it when the copy is opened, so move or copy those files together. A later restore replaces all three.

**What this does not prove is who wrote the file.** age encrypts to a public key, so anyone holding that key can build a complete, valid backup with whatever data they like. A file replaced on the backup host restores without complaint, so the off-site host must be trusted not to swap files, or you need your own record of each backup's hash. To limit the damage, restore installs only blobs a document references, and it clears the API token and all OAuth clients and grants: afterwards every client authorizes again and the API token must be reissued. `--keep-credentials` keeps them, for a backup you know you wrote, such as your own pre-upgrade one. A forged database could still hold rows or triggers that restore doesn't look for.

Restoring over the live data: `sudo systemctl stop nexus`, run `nexus restore ... --data-dir /var/lib/nexus --force` as the `nexus` user (copy the key to a temporary file readable only by that user, and delete it afterwards), then `sudo systemctl start nexus`.

## 7. Ingress

`nexus serve` binds `127.0.0.1:8080` and should stay on loopback; something on the board forwards to it. Claude.ai needs a public HTTPS URL. Test both options; the M1 gate is a full Claude.ai OAuth and tool call through whichever one is chosen (SPEC section 8).

Whichever is used, set `NEXUS_TRUSTED_IP_HEADER` for it (section 3) and restart before going public.

**Buffering.** The MCP router is mounted, and this is what its responses really carry. `POST` returns one complete `application/json` body with `Content-Length`, and no `Cache-Control`. `GET` returns `text/event-stream` with `Cache-Control: no-cache, no-transform` and an empty body that ends at once. Nothing sets `X-Accel-Buffering`. So there is no long-lived stream for a proxy to buffer, and that header shouldn't be needed. That is an expectation, not a result: nothing has been tested through either ingress, and only a real Claude.ai OAuth and tool call can settle it. The steps are in [`docs/CUTOVER.md`](../docs/CUTOVER.md), step 4.

- **Cloudflare tunnel** (`cloudflared` on the board, ingress `http://127.0.0.1:8080`): keeps a stable custom domain, but a tunnel is always proxied. Old Nexus's local notes say Cloudflare's proxy broke MCP Streamable HTTP by buffering streaming responses, so its domain stayed DNS-only.
- **Tailscale Funnel** (`tailscale funnel 8080`): no Cloudflare in the path, but the hostname becomes `*.ts.net`, so every OAuth client re-registers once.

`NEXUS_PUBLIC_BASE_URL` must be the public origin. It is the OAuth issuer, so changing it later means every client registers again. Calendar apps that cannot send bearer tokens use the secret URL; Google Calendar cannot reach a tailnet-only URL, so use an on-device subscriber (ICSx5) or the public ingress.

### Limits

Each kind of caller has its own pool of request slots, so anonymous traffic can't starve a logged-in owner or an agent. A request that finds its pool full waits, then gets a 503 with `Retry-After: 5`, logged as `overloaded`. Credentials are checked before a slot is taken. `/healthz` is outside all of this, so **a green health check doesn't mean the pools have room**: watch for `overloaded`.

| Pool | Routes | In flight | Longest wait | Deadline, body read included |
|---|---|---|---|---|
| public | login, OAuth, calendar | 8 | 2 s | 10 s |
| owner | dashboard, JSON API | 8 | 10 s | 60 s |
| mcp | `/api/mcp`, `/mcp` | 8 | 10 s | 60 s; 300 s for a large body |

Token refreshes and logins share the public pool, so a flood there can delay them even though it can't touch the other two. They get a 503 rather than hanging, and clients with a valid access token carry on working.

Moving a whole document is expensive: the transport holds several copies, about 85 MB for one 10 MiB document, measured with the musl build. So on the MCP route, a body over 1 MiB or of unknown length, and any `download_document_content` call, needs one of **2** large slots, held until the response has been written, waiting up to 30 s for it. A batch asking for more than 2 downloads gets a 413. These slots keep one token holder from exhausting `MemoryMax`. A client that stops reading a download holds its slot until its connection ends, which blocks other large calls but nothing else.

A large upload has 300 s, so 14 MB needs about 373 kbit/s, and must not stall for more than 30 s at a time. Anything slower gets a 408. **The ingress may be stricter:** check that it allows an upload this long.

Connections: at most 256 at once, and past that a new one is closed at once. Headers must arrive within 10 s and fit in 64 KiB. An idle keep-alive connection is closed after 10 s too, which a reverse proxy handles by opening another.

## 8. Upgrade and rollback

Upgrade: `sudo systemctl start nexus-backup.service`, install the new binary (keeping `nexus.prev`), `sudo systemctl restart nexus`, check healthz.

Rollback: `sudo systemctl stop nexus`, `sudo install -m 0755 /usr/local/bin/nexus.prev /usr/local/bin/nexus`, start. Database migrations only move forward. If the new version migrated the database, an older binary may refuse it; restore the pre-upgrade backup (section 6) with `--force --keep-credentials`.

## 9. Cutover checklist (SPEC section 7)

The steps, commands and rollback are in [`docs/CUTOVER.md`](../docs/CUTOVER.md). In short:

0. Owner passphrase set, `NEXUS_PUBLIC_BASE_URL` and `NEXUS_TRUSTED_IP_HEADER` set, and a failed login from outside logs your real address. Replay the slow-request checks through the chosen ingress.
1. Board health: `journalctl -b | grep end0` shows no Ethernet drops (SPEC section 10), `free -h` has headroom, and a reboot with `/home` locked brings Nexus up.
2. Restore drill passed with the real backup key; off-site hook in place or the gap accepted in writing.
3. Ingress chosen and tested end to end with Claude.ai (OAuth and a tool call), plus Hermes. Note the redirect host Claude.ai really uses, and set `NEXUS_OAUTH_REDIRECT_HOSTS` if the log shows `redirect_host_rejected` (section 3).
4. Stop writes to old Nexus.
5. Re-export from old Nexus (table allowlist, mode 700 directory), copy it to the board, run the import (section 4).
6. Re-run the parity gate against the imported data.
7. Take a backup and confirm it restores.
8. Repoint clients. Claude.ai and Hermes each re-authorise once, because the OAuth tables start empty.
9. Keep old Nexus stopped but restorable for 30 days; review its `_nexus-backups/` bucket content before decommissioning.
