#!/usr/bin/env bash
# Runs on the board as root, fed by deploy/promote. Snapshot, swap the binary, restart, check, and
# go back to the previous binary if production does not answer.
# Not promoted: changes to deploy/nexus.service. Install those by hand (README section 2).
set -euo pipefail
umask 077
share=/usr/local/share/nexus
rev=$(cat "$share/staging.rev" 2>/dev/null || echo unknown)

if cmp -s /usr/local/bin/nexus-staging /usr/local/bin/nexus; then
    echo "production already runs $rev"
    exit 0
fi
curl -fsS -o /dev/null http://127.0.0.1:8092/healthz \
    || { echo "staging is not healthy: nothing promoted" >&2; exit 1; }

# Migrations only move forward, so the snapshot is what a rollback past one needs.
snap=/var/lib/nexus/pre-upgrade-$(date -u +%Y%m%dT%H%M%SZ)
install -d -m 0700 -o nexus -g nexus "$snap"
sudo -u nexus python3 -c '
import sqlite3, sys
src = sqlite3.connect("file:/var/lib/nexus/nexus.db?mode=ro", uri=True)
dst = sqlite3.connect(sys.argv[1] + "/nexus.db")
src.backup(dst)
' "$snap"

install -m 0755 /usr/local/bin/nexus /usr/local/bin/nexus.prev
install -m 0755 /usr/local/bin/nexus-staging /usr/local/bin/nexus
systemctl restart nexus

for _ in $(seq 30); do
    if curl -fsS -o /dev/null http://127.0.0.1:8091/healthz; then
        echo "$rev" > "$share/prod.rev"
        chmod 0644 "$share/prod.rev"
        # Each snapshot is a plain copy of the database: the newest three are enough to go back.
        ls -1d /var/lib/nexus/pre-upgrade-2* | sort | head -n -3 | xargs -r rm -rf --
        echo "production runs $rev. Snapshot: $snap"
        exit 0
    fi
    sleep 1
done

echo "production did not come up: going back to the previous binary" >&2
journalctl -u nexus -n 15 --no-pager >&2
install -m 0755 /usr/local/bin/nexus.prev /usr/local/bin/nexus
systemctl restart nexus
sleep 3
if curl -fsS -o /dev/null http://127.0.0.1:8091/healthz; then
    echo "rolled back, production answers again" >&2
else
    echo "ROLLBACK FAILED TOO. If the new binary migrated the database, restore $snap/nexus.db" >&2
fi
exit 1
