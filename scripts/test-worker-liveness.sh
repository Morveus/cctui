#!/usr/bin/env sh
# Exercise worker-entrypoint.sh's probe_session against synthetic roster payloads.
#
# The function text is extracted from the entrypoint rather than copied, so a
# change to the shipped decision logic is what gets tested. `inactive` is
# ambiguous on the wire (real SessionEnded vs a heartbeat merely aged past the
# server's window) and reading it as death aborts live workers mid-tool-call, so
# the ambiguity resolution is pinned here.
#
# Runs under `sh`, matching the entrypoint's own shebang — the function must stay
# POSIX, and bash would hide a bashism that breaks in the image's dash.
set -eu

_root=$(cd "$(dirname "$0")/.." && pwd)
_entrypoint="$_root/deploy/worker-entrypoint.sh"
_work=$(mktemp -d)
trap 'rm -rf "$_work"' EXIT

log() { :; }
curl() { echo 200; }

SESSION_ID="356d4dde-659c-47c7-8a3c-aa4e5c44b50a"
CCTUI_MACHINE_KEY=unused
_SESSIONS_URL=http://unused
_PROBE_BODY="$_work/roster.json"
_PROBE_LOGGED_CODE=""
_PROBE_LOGGED_NOHB=""
WORKER_SERVER_STATUS_WINDOW_SECS=300
WORKER_LIVENESS_STALE_SECS=1800

eval "$(awk '/^probe_session\(\) \{/,/^\}/' "$_entrypoint")"

_fails=0
expect() {
    _want=$1 _desc=$2 _got=$(probe_session)
    if [ "$_got" = "$_want" ]; then
        printf 'ok   %s (%s)\n' "$_desc" "$_got"
    else
        printf 'FAIL %s: want %s, got %s\n' "$_desc" "$_want" "$_got"
        _fails=$((_fails + 1))
    fi
}

# Roster carrying our session with $1 status and a heartbeat $2 seconds old.
roster() {
    _hb=$(date -u -d "@$(( $(date -u +%s) - $2 ))" +%Y-%m-%dT%H:%M:%S.123456Z)
    printf '{"sessions":[{"id":"%s","status":"%s","last_heartbeat":"%s"}]}\n' \
        "$SESSION_ID" "$1" "$_hb" > "$_PROBE_BODY"
}

roster active 5;        expect registered "live session"
roster new 5;           expect registered "session still booting"
roster inactive 10;     expect ended      "deregistered: inactive with a fresh heartbeat"
roster inactive 400;    expect quiet      "quiet: past the server window, inside our slack"
roster inactive 1700;   expect quiet      "quiet: still inside our slack"
roster inactive 1900;   expect ended      "gone: heartbeat past WORKER_LIVENESS_STALE_SECS"

# The regression this guards: a tool call blocking well past the server's 5m
# window must NOT read as death.
roster inactive 500;    expect quiet      "8m blocking tool call survives"

printf '{"sessions":[{"id":"%s","status":"inactive","last_heartbeat":null}]}\n' \
    "$SESSION_ID" > "$_PROBE_BODY"
expect ended "unreadable heartbeat falls back to fail-closed"

printf '{"sessions":[]}\n' > "$_PROBE_BODY"
expect unknown "our id absent from the roster is never death"

curl() { return 7; }
expect unknown "curl failure is never death"

[ "$_fails" -eq 0 ] || { printf '\n%d assertion(s) failed\n' "$_fails"; exit 1; }
printf '\nall assertions passed\n'
