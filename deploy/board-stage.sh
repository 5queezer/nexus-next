#!/usr/bin/env bash
# Runs on the board as root, fed by deploy/stage. Installs /tmp/nexus-staging.new as the staging
# service. Production is only read: its database is copied with SQLite's backup, never touched.
set -euo pipefail
umask 077
rev=$1
fresh=$2
prod=/var/lib/nexus
data=/var/lib/nexus-staging
# This board's own tailnet name, so that no file holds the address.
url=https://$(tailscale status --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))'):8445

install -m 0755 /tmp/nexus-staging.new /usr/local/bin/nexus-staging
rm -f /tmp/nexus-staging.new

# The staging unit is production's unit with its paths and port swapped, so the two can't drift.
sed -e 's|^Description=.*|Description=Nexus staging|' \
    -e 's|^StateDirectory=nexus$|StateDirectory=nexus-staging|' \
    -e 's|/etc/nexus/|/etc/nexus-staging/|g' \
    -e 's|/usr/local/bin/nexus serve --bind 127.0.0.1:8091 --data-dir /var/lib/nexus |/usr/local/bin/nexus-staging serve --bind 127.0.0.1:8092 --data-dir /var/lib/nexus-staging |' \
    /tmp/nexus-staging.unit > /tmp/nexus-staging.service
rm -f /tmp/nexus-staging.unit
grep -q 'nexus-staging serve --bind 127.0.0.1:8092 --data-dir /var/lib/nexus-staging ' /tmp/nexus-staging.service \
    || { echo "the unit's ExecStart changed: update the sed in deploy/board-stage.sh" >&2; exit 1; }
install -m 0644 /tmp/nexus-staging.service /etc/systemd/system/nexus-staging.service
rm -f /tmp/nexus-staging.service

if [ ! -d /etc/nexus-staging ]; then
    install -d -m 0750 /etc/nexus-staging
    printf '%s\n' "NEXUS_PUBLIC_BASE_URL=$url" \
        'NEXUS_TRUSTED_IP_HEADER=X-Forwarded-For' | install -m 0640 /dev/stdin /etc/nexus-staging/nexus.env
    openssl rand -hex 32 | install -m 0600 /dev/stdin /etc/nexus-staging/calendar-token
fi
grep -q '^NEXUS_ENV_LABEL=' /etc/nexus-staging/nexus.env || echo 'NEXUS_ENV_LABEL=Staging' >> /etc/nexus-staging/nexus.env

systemctl stop nexus-staging 2>/dev/null || true
grep -q '^NEXUS_OWNER_ID=' /etc/nexus-staging/nexus.env \
    || printf '%s\n' 'NEXUS_OWNER_ID=staging-owner' 'NEXUS_OWNER_EMAIL=owner@example.org' >> /etc/nexus-staging/nexus.env
if [ "$fresh" = from-prod ]; then
    # Real data and the real passphrase, for a check that needs them. Run --fresh afterwards.
    install -d -m 0700 -o nexus -g nexus "$data"
    rm -f "$data/nexus.db" "$data/nexus.db-wal" "$data/nexus.db-shm"
    sudo -u nexus python3 -c '
import sqlite3, sys
src = sqlite3.connect("file:" + sys.argv[1] + "/nexus.db?mode=ro", uri=True)
dst = sqlite3.connect(sys.argv[2] + "/nexus.db")
src.backup(dst)
' "$prod" "$data"
    rsync -a --delete "$prod/blobs/" "$data/blobs/"
    install -m 0600 -o nexus -g nexus "$prod/owner-passphrase.argon2" "$data/owner-passphrase.argon2"
    echo "staging data: a copy of production. It is real data behind a second login: run deploy/stage --fresh when done."
elif [ "$fresh" = yes ] || [ ! -f "$data/nexus.db" ]; then
    # Empty, with a passphrase of its own: a guess at staging's login must say nothing about production's.
    install -d -m 0700 -o nexus -g nexus "$data"
    rm -rf "$data/nexus.db" "$data/nexus.db-wal" "$data/nexus.db-shm" "$data/blobs" "$data/cache" "$data/owner-passphrase.argon2" "$data/openai-api-key"
    pass=$(openssl rand -base64 24)
    printf '%s\n' "$pass" | sudo -u nexus /usr/local/bin/nexus-staging set-passphrase --data-dir "$data" >/dev/null
    echo "staging data: empty. Its passphrase, shown once: $pass"
fi

install -d -m 0755 /usr/local/share/nexus
echo "$rev" > /usr/local/share/nexus/staging.rev
chmod 0644 /usr/local/share/nexus/staging.rev

systemctl daemon-reload
systemctl enable --quiet nexus-staging
systemctl restart nexus-staging
tailscale serve --bg --https=8445 http://127.0.0.1:8092 >/dev/null

for _ in $(seq 20); do
    if curl -fsS -o /dev/null http://127.0.0.1:8092/healthz; then
        echo "staging runs $rev: $url"
        exit 0
    fi
    sleep 1
done
echo "staging did not come up:" >&2
journalctl -u nexus-staging -n 15 --no-pager >&2
exit 1
