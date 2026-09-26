#!/usr/bin/env bash
# Exercises pack_wire_instructions() from deploy/worker-entrypoint.sh against a
# fake context dir + workspace. Extracts the function so the entrypoint's main()
# never runs.
set -u

ENTRY="${1:-$(cd "$(dirname "$0")/.." && pwd)/worker-entrypoint.sh}"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fail=0
ok()   { printf '  ok   %s\n' "$1"; }
bad()  { printf '  FAIL %s\n' "$1"; fail=1; }
check(){ if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }

eval "$(awk '/^pack_dir_src\(\)/,/^}/' "$ENTRY")"
eval "$(awk '/^pack_wire_instructions\(\)/,/^}/' "$ENTRY")"
log() { :; }
WORKER_UID=$(id -u)

setup() {
  rm -rf "$T/ctx" "$T/workspace"
  mkdir -p "$T/ctx" "$T/workspace"
  CONTEXT_DIR="$T/ctx"
}
run() { PACK_WORKSPACE_DIR="$T/workspace" pack_wire_instructions; }

echo "== 1. AGENTS.md + CLAUDE.md pack (the new _base shape) =="
setup
printf 'BASE INSTRUCTIONS\n' > "$CONTEXT_DIR/AGENTS.md"
printf '@AGENTS.md\n'        > "$CONTEXT_DIR/CLAUDE.md"
_dirs="projects projects"; TASK_REPO=ownrepo
run
check "AGENTS.md staged"  "BASE INSTRUCTIONS" "$(cat "$T/workspace/AGENTS.md")"
check "CLAUDE.md imports" "@AGENTS.md"        "$(cat "$T/workspace/CLAUDE.md")"

echo "== 2. legacy pack: CLAUDE.md only =="
setup
printf 'LEGACY\n' > "$CONTEXT_DIR/CLAUDE.md"
_dirs=""; TASK_REPO=""
run
check "AGENTS.md from CLAUDE.md" "LEGACY"     "$(cat "$T/workspace/AGENTS.md")"
check "CLAUDE.md synthesized"    "@AGENTS.md" "$(cat "$T/workspace/CLAUDE.md")"

echo "== 3. projects/<repo> overlay appended =="
setup
printf 'BASE\n' > "$CONTEXT_DIR/AGENTS.md"
printf '@AGENTS.md\n' > "$CONTEXT_DIR/CLAUDE.md"
mkdir -p "$CONTEXT_DIR/projects/workrepo"
printf 'REPO OVERLAY\n' > "$CONTEXT_DIR/projects/workrepo/CLAUDE.md"
_dirs="projects projects"; TASK_REPO=workrepo
run
check "overlay appended" "BASE

REPO OVERLAY" "$(cat "$T/workspace/AGENTS.md")"

echo "== 4. overlay for a DIFFERENT repo is not appended =="
setup
printf 'BASE\n' > "$CONTEXT_DIR/AGENTS.md"
mkdir -p "$CONTEXT_DIR/projects/workrepo"
printf 'REPO OVERLAY\n' > "$CONTEXT_DIR/projects/workrepo/CLAUDE.md"
_dirs="projects projects"; TASK_REPO=ownrepo
run
check "no cross-repo leak" "BASE" "$(cat "$T/workspace/AGENTS.md")"

echo "== 5. pack with no instructions writes nothing =="
setup
_dirs=""; TASK_REPO=ownrepo
run
check "no AGENTS.md" "absent" "$([ -e "$T/workspace/AGENTS.md" ] && echo present || echo absent)"
check "no CLAUDE.md" "absent" "$([ -e "$T/workspace/CLAUDE.md" ] && echo present || echo absent)"

echo "== 6. nothing is written inside the checkout =="
setup
printf 'BASE\n' > "$CONTEXT_DIR/AGENTS.md"
mkdir -p "$T/workspace/ownrepo"
printf 'REPO OWN\n' > "$T/workspace/ownrepo/AGENTS.md"
_dirs=""; TASK_REPO=ownrepo
run
check "repo AGENTS.md untouched" "REPO OWN" "$(cat "$T/workspace/ownrepo/AGENTS.md")"

echo
[ "$fail" = 0 ] && echo "ALL PASS" || echo "FAILURES"
exit "$fail"
