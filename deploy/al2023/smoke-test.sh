#!/usr/bin/env bash
# Run on a fresh AL2023 installation, before starting the installed unit.
set -euo pipefail

fail() { printf 'Smoke test failed: %s\n' "$*" >&2; exit 1; }

[[ $EUID == 0 ]] || fail 'run with sudo on a disposable AL2023 host'
source /etc/os-release
[[ $ID == amzn && $VERSION_ID == 2023 ]] || fail 'expected Amazon Linux 2023'
dir=$(cd -- "$(dirname -- "$0")" && pwd)
cmp -s "$dir/ragios.yaml" /etc/ragios/ragios.yaml || fail 'install the unmodified example configuration'
[[ $(systemctl show -p ActiveState --value ragios.service) == inactive ]] || fail 'unit must be inactive'
db=/var/lib/ragios/ragios.sqlite3
for path in "$db" "$db-wal" "$db-shm"; do
    [[ ! -e $path && ! -L $path ]] || fail 'requires fresh state; do not delete a fleet database'
done

base=http://127.0.0.1:8080
fetch() { curl --noproxy '*' --fail --silent --max-time 2 "$base$1"; }

# Read the result through the dashboard, not the database. An unchanged finish
# time after restart proves this is the saved result, not a new plugin run.
finished() {
    local page
    page=$(fetch "$1") || return 1
    grep -Fq "$2" <<< "$page" || return 1
    grep -oE '<th>Finished \(UTC\)</th><td>[^<]+' <<< "$page"
}

snapshot() {
    finished /hosts/local/services/dashboard/status '<th>Latest result</th><td class="OK">OK</td>' &&
    finished /hosts/local/services/dashboard-tcp/status '<th>Latest result</th><td class="OK">OK</td>' &&
    finished /hosts/local/reachability/status '<th>Latest result</th><td class="OK">Reachable (OK)'
}

wait_for_results() {
    local result deadline=$((SECONDS + 45))
    until result=$(snapshot); do
        (( SECONDS < deadline )) || fail 'check results did not become healthy; inspect dashboard and journalctl -u ragios'
        sleep 1
    done
    printf '%s\n' "$result"
}

systemctl start ragios.service
before=$(wait_for_results)
pid=$(systemctl show -p MainPID --value ragios.service)
uid=$(id -u ragios)
[[ $uid != 0 ]]
awk -v uid="$uid" '
    $1 == "Uid:" { user_ok = ($2 == uid && $3 == uid && $4 == uid && $5 == uid) }
    $1 == "NoNewPrivs:" { no_new_privs = ($2 == 1) }
    $1 == "CapEff:" { effective_empty = ($2 ~ /^0+$/) }
    $1 == "CapBnd:" { bounding_empty = ($2 ~ /^0+$/) }
    END { exit !(user_ok && no_new_privs && effective_empty && bounding_empty) }
' "/proc/$pid/status" || fail 'server is not running as ragios without capabilities'
[[ -s $db && $(stat -c %U "$db") == ragios ]] || fail 'missing or misowned SQLite database'
fetch /assets/htmx.min.js | cmp - "$dir/../../assets/htmx.min.js" || fail 'bundled asset differs from source'
printf 'OK: unprivileged process, HTTP and TCP service checks, ping reachability, and bundled HTMX\n'

systemctl restart ragios.service
after=$(wait_for_results)
[[ $before == "$after" ]] || fail 'result timestamps changed after restart'
printf 'OK: all three check results persisted across systemd restart\n'
