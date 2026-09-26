#!/usr/bin/env bash
# CODEX_VERSION is a FLOOR, not an exact pin: derived images (the harbor worker
# bake) refetch the harness, so the repo cannot promise which build ships —
# only that it is never older than the declared version. The floor lives in
# contract::CODEX_MIN_VERSION (the build the retained JSON Schema was generated
# from) and the worker image ARG CODEX_VERSION must equal it, so the base image
# installs exactly the build the schema documents.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
contract="${1:-$repo_root/crates/cctui-daemon/src/adapters/codex/contract.rs}"
dockerfile="${2:-$repo_root/deploy/worker.Dockerfile}"

fail() { echo "::error::$*" >&2; exit 1; }

# True when $1 is at least $2.
at_least() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }

[ -f "$contract" ]   || fail "contract file not found: $contract"
[ -f "$dockerfile" ] || fail "Dockerfile not found: $dockerfile"

floor="$(sed -n 's/.*CODEX_MIN_VERSION: &str = "\([^"]*\)".*/\1/p' "$contract" | head -n1)"
docker_version="$(sed -n 's/^ARG CODEX_VERSION=\(.*\)$/\1/p' "$dockerfile" | head -n1)"

[ -n "$floor" ]          || fail "could not read CODEX_MIN_VERSION from $contract"
[ -n "$docker_version" ] || fail "could not read ARG CODEX_VERSION from $dockerfile"

echo "contract CODEX_MIN_VERSION (floor) = $floor"
echo "Dockerfile ARG CODEX_VERSION       = $docker_version"

case "$floor" in
  latest | stable)
    fail "CODEX_MIN_VERSION is floating ('$floor'). Declare a concrete x.y.z floor."
    ;;
esac

echo "$floor" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || fail "CODEX_MIN_VERSION '$floor' is not a concrete x.y.z version."

[ "$floor" = "$docker_version" ] \
  || fail "Codex floor drift: contract=$floor vs Dockerfile=$docker_version. Update both and regenerate the retained schema."

installed="${CODEX_INSTALLED_VERSION:-}"
if [ -z "$installed" ] && command -v codex >/dev/null 2>&1; then
  installed="$(codex --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
fi

if [ -n "$installed" ]; then
  echo "installed codex                    = $installed"
  at_least "$installed" "$floor" \
    || fail "installed Codex $installed is below the declared floor $floor."
  echo "OK: the installed harness satisfies the floor."
else
  echo "no codex binary to check; floor consistency check only."
fi

upstream="$(curl -fsSL https://api.github.com/repos/openai/codex/releases/latest 2>/dev/null \
  | sed -n 's/.*"tag_name": *"rust-v\([0-9.]*\)".*/\1/p' | head -n1 || true)"
if [ -z "$upstream" ]; then
  echo "warning: could not fetch the upstream latest version."
  exit 0
fi

echo "upstream latest                    = $upstream"
if at_least "$floor" "$upstream"; then
  echo "OK: the floor is at the current upstream latest ($floor)."
  exit 0
fi

echo "::notice::Codex floor $floor is behind upstream latest $upstream."
echo "Raise CODEX_MIN_VERSION, ARG CODEX_VERSION, regenerate the retained schema, re-vendor codex-config.schema.json and re-verify codex-catalog.toml (scripts/check-codex-schema-drift.sh) when workers must not run anything older."
