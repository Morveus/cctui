#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/check-codex-schema-drift.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/curated.rs" <<'RS'
pub const CURATED: &[Curated] = &[
    k("hide_agent_reasoning", TomlType::Bool),
    k("history.persistence", TomlType::Str),
    k("model_context_window", TomlType::Int),
];
RS

catalog() {
  {
    printf '# Verified against codex-cli %s.\n\n' "$1"
    printf '[[keys]]\nname = "hide_agent_reasoning"\ntag = "safe"\nsource = "schema"\ntype = "boolean"\n\n'
    printf '[[keys]]\nname = "history.persistence"\ntag = "care"\nsource = "schema"\ntype = "string"\n\n'
    printf '[[keys]]\nname = "model_context_window"\ntag = "care"\nsource = "schema"\ntype = "number"\n\n'
    printf '[[keys]]\nname = "model_provider"\ntag = "managed"\nsource = "schema"\n\n'
    printf '[[presets]]\nid = "quiet"\nname = "type = \\"x\\""\n'
  } > "$tmp/catalog.toml"
}

printf 'pub const CODEX_MIN_VERSION: &str = "0.153.4";\n' > "$tmp/contract.rs"

schema() {
  local provider='"model_provider": {"type": "string"},'
  [ "${2:-yes}" = yes ] || provider=''
  cat > "$tmp/schema.json" <<JSON
{
  "properties": {
    "hide_agent_reasoning": {"type": "$1"},
    $provider
    "history": {"allOf": [{"\$ref": "#/definitions/History"}]},
    "model_context_window": {"type": ["integer", "null"], "format": "int64"}
  },
  "definitions": {
    "History": {"properties": {"persistence": {"\$ref": "#/definitions/Persistence"}}},
    "Persistence": {"oneOf": [{"enum": ["save-all"], "type": "string"}, {"enum": ["none"]}]}
  }
}
JSON
}

run() { bash "$script" "$tmp/schema.json" "$tmp/catalog.toml" "$tmp/curated.rs" "$tmp/contract.rs"; }

failures=0
expect() {
  local want="$1" name="$2"
  local out status
  set +e
  out="$(run 2>&1)"; status=$?
  set -e
  if [ "$status" -eq "$want" ]; then
    echo "ok   — $name"
  else
    echo "FAIL — $name (exit $status, expected $want)"
    echo "$out" | sed 's/^/       /'
    failures=$((failures + 1))
  fi
}

catalog 0.153.4
schema boolean
expect 0 "matching keys, \$ref-dotted paths and nullable integers pass"

schema string
expect 1 "a retyped curated key fails"

schema boolean no
expect 1 "a managed key dropped upstream fails"

schema boolean
catalog 0.144.1
expect 1 "a catalog verified against another version than the floor fails"

[ "$failures" -eq 0 ] || exit 1
echo "all schema-drift cases passed"
