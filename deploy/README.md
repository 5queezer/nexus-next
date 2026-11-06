# Deploying Nexus on the ROCK 4 SE

Target: Armbian (Debian 13), aarch64, systemd. Data lives on the root filesystem in `/var/lib/nexus`, never in `/home`, so the service starts unattended after a reboot while `/home` is still locked.

Files here:

| File | Install to |
|---|---|
| `nexus.service` | `/etc/systemd/system/` |
| `nexus-backup.service`, `nexus-backup.timer` | `/etc/systemd/system/` |
| `nexus.sysusers` | `/etc/sysusers.d/nexus.conf` |

## What this build serves

Until the auth layer is mounted, `nexus serve` mounts only `GET /healthz` and, if a calendar token is configured, `/calendar.ics`. The dashboard and JSON API are refused unless `--insecure-local-only` is passed, which also forces a loopback bind. Do not add that flag to the unit on the board. The unit therefore serves nothing useful to humans yet; that is intended.

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

The owner passphrase for the OAuth login page arrives with the auth layer. It will be one more credential file in the same directory.

Optional settings (public origin, bind address) go in `/etc/nexus/nexus.env`, for example:

```
NEXUS_BASE_URL=https://nexus.example.net
```

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

The import prints counts and fails on any count, hash or foreign-key mismatch. Re-running it is safe; `--force` replaces a database holding rows the import did not write.

## 5. Start

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now nexus.service nexus-backup.timer
curl -fsS http://127.0.0.1:8080/healthz        # ok
sudo journalctl -u nexus -n 20 --no-pager
systemctl list-timers nexus-backup.timer
```

Logs carry method, route template and status only. Query strings, headers and bodies are never logged. `NEXUS_LOG_LEVEL` (default `info`) only changes verbosity; SQL text stays out at every level.

Reboot test, with `/home` left locked: `sudo reboot`, wait, then `systemctl is-active nexus` and the healthz call above must succeed without anyone unlocking anything.

## 6. Backups

`nexus-backup.timer` runs at 03:30 UTC (plus up to 10 minutes of jitter) and writes `/var/lib/nexus-backups/nexus-<UTC timestamp>.tar.age`, then deletes all but the newest 14. Each file is one age-encrypted tar of a consistent `VACUUM INTO` snapshot plus every blob the snapshot references. Before packaging, the copy passes `integrity_check` and `foreign_key_check`, and every referenced blob must exist and match its hash; otherwise the run fails and leaves no file. It is safe while the server runs.

Run one now: `sudo systemctl start nexus-backup.service`. Run one before every upgrade.

### Off-site sync

The destination is not decided. The files are already encrypted, so any copy tool is safe. Add a drop-in with the hook once there is somewhere to send them:

```sh
sudo systemctl edit nexus-backup.service
```

```ini
[Service]
# The backup unit has no network access by default; a sync tool needs it.
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
# Runs only after a successful backup. Example with rsync over ssh (key readable by the nexus user):
ExecStartPost=/usr/bin/rsync -a --ignore-existing -e "ssh -i /etc/nexus/offsite-key" /var/lib/nexus-backups/ backup@host:nexus/
```

A failing hook marks the unit failed, which shows in `systemctl --failed`.

### Restore drill (workstation or board)

```sh
nexus restore --from nexus-20261107T033000Z.tar.age --identity nexus-backup.key --data-dir /tmp/nexus-drill
nexus serve --data-dir /tmp/nexus-drill --bind 127.0.0.1:8099 --insecure-local-only   # look at it, then Ctrl-C
```

Restore verifies everything before touching the target: decryption (a wrong key, truncation or tampering installs nothing), `integrity_check`, `foreign_key_check`, and every blob against its hash. It refuses to overwrite an existing `nexus.db` without `--force`; with `--force` the old file is kept as `nexus.db.before-restore`.

Restoring over the live data: `sudo systemctl stop nexus`, run `nexus restore ... --data-dir /var/lib/nexus --force` as the `nexus` user (copy the key to a temporary file readable only by that user, and delete it afterwards), then `sudo systemctl start nexus`.

## 7. Ingress

`nexus serve` binds `127.0.0.1:8080` and should stay on loopback; something on the board forwards to it. Claude.ai needs a public HTTPS URL. Test both options; the M1 gate is a full Claude.ai OAuth and tool call through whichever one is chosen (SPEC section 8).

- **Cloudflare tunnel** (`cloudflared` on the board, ingress `http://127.0.0.1:8080`): keeps a stable custom domain. Local notes say Cloudflare's proxy breaks MCP Streamable HTTP by buffering streaming responses unless buffering is disabled. The MCP routes must therefore send `X-Accel-Buffering: no` and `Cache-Control: no-store`. This server does not set those yet because the MCP router is not mounted; check when it lands.
- **Tailscale Funnel** (`tailscale funnel 8080`): no Cloudflare in the path, but the hostname becomes `*.ts.net`, so every OAuth client re-registers once.

Set `NEXUS_BASE_URL` to the public origin. Calendar apps that cannot send bearer tokens use the secret URL; Google Calendar cannot reach a tailnet-only URL, so use an on-device subscriber (ICSx5) or the public ingress.

## 8. Upgrade and rollback

Upgrade: `sudo systemctl start nexus-backup.service`, install the new binary (keeping `nexus.prev`), `sudo systemctl restart nexus`, check healthz.

Rollback: `sudo systemctl stop nexus`, `sudo install -m 0755 /usr/local/bin/nexus.prev /usr/local/bin/nexus`, start. Database migrations only move forward. If the new version migrated the database, an older binary may refuse it; restore the pre-upgrade backup (section 6) with `--force`.

## 9. Cutover checklist (SPEC section 7)

1. Board health: `journalctl -b | grep end0` shows no Ethernet drops (SPEC section 10), `free -h` has headroom, and a reboot with `/home` locked brings Nexus up.
2. Restore drill passed with the real backup key; off-site hook in place or the gap accepted in writing.
3. Ingress chosen and tested end to end with Claude.ai (OAuth and a tool call), plus Hermes.
4. Stop writes to old Nexus.
5. Re-export from old Nexus (table allowlist, mode 700 directory), copy it to the board, run the import (section 4).
6. Re-run the parity gate against the imported data.
7. Take a backup and confirm it restores.
8. Repoint clients. Claude.ai and Hermes each re-authorise once, because the OAuth tables start empty.
9. Keep old Nexus stopped but restorable for 30 days; review its `_nexus-backups/` bucket content before decommissioning.
