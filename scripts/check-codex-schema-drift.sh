#!/usr/bin/env bash
# Expected types come from CURATED first, then the catalog's own `type`, so this
# adds no third source of truth. The catalog header version must equal
# CODEX_MIN_VERSION: schema, catalog and floor name one release.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
schema="${1:-$repo_root/crates/cctui-server/src/settings_catalog/codex-config.schema.json}"
catalog="${2:-$repo_root/crates/cctui-server/src/settings_catalog/codex-catalog.toml}"
curated="${3:-$repo_root/crates/cctui-proto/src/codex_config.rs}"
contract="${4:-$repo_root/crates/cctui-daemon/src/adapters/codex/contract.rs}"

fail() { echo "::error::$*" >&2; exit 1; }

for f in "$schema" "$catalog" "$curated" "$contract"; do
  [ -f "$f" ] || fail "file not found: $f"
done

floor="$(sed -n 's/.*CODEX_MIN_VERSION: &str = "\([^"]*\)".*/\1/p' "$contract" | head -n1)"
verified="$(sed -n 's/^# Verified against codex-cli \([0-9][0-9.]*[0-9]\)\.*$/\1/p' "$catalog" | head -n1)"
[ -n "$floor" ]    || fail "could not read CODEX_MIN_VERSION from $contract"
[ -n "$verified" ] || fail "could not read '# Verified against codex-cli X' from $catalog"
[ "$floor" = "$verified" ] \
  || fail "catalog verified against codex $verified but CODEX_MIN_VERSION is $floor; re-verify the catalog at the floor."
echo "checking codex-catalog.toml against ${SCHEMA_LABEL:-$schema} (floor $floor)"

spec="$(
  sed -n 's/.*k("\([^"]*\)", TomlType::\([A-Za-z]*\)).*/C\t\1\t\2/p' "$curated"
  awk '
    /^\[\[/ { if (name != "") print "K\t" name "\t" ty; name = ""; ty = ""; inkey = ($0 == "[[keys]]"); next }
    inkey && /^name = / { name = $3; gsub(/"/, "", name) }
    inkey && /^type = / { ty = $3; gsub(/"/, "", ty) }
    END { if (name != "") print "K\t" name "\t" ty }
  ' "$catalog"
)"

report="$(jq -r --arg spec "$spec" '
  . as $root
  | def node: if type != "object" then .
      elif has("$ref") then $root.definitions[.["$ref"] | sub("^#/definitions/"; "")] | node
      elif (.allOf | length) == 1 then .allOf[0] | node
      else . end;
    def types: node
      | if (.type | type) == "string" then [.type]
        elif (.type | type) == "array" then .type
        elif has("oneOf") or has("anyOf") then [(.oneOf // .anyOf)[] | types[]]
        elif has("enum") then [.enum[] | type]
        else [] end
      | map(select(. != "null")) | unique;
    def at($path): reduce ($path | split("."))[] as $s (.;
      if . == null then null else (node | .properties[$s]) end);
    def want($t): {Str: "string", Bool: "boolean", Int: "integer",
      string: "string", boolean: "boolean", number: "integer"}[$t] // "";
  ($spec | split("\n") | map(select(. != "") | split("\t"))) as $rows
  | ($rows | map(select(.[0] == "C")) | map({key: .[1], value: want(.[2])}) | from_entries) as $cur
  | $rows[] | select(.[0] == "K") | .[1] as $name
  | ($cur[$name] // want(.[2] // "")) as $w
  | ($root | at($name)) as $n
  | if $n == null then "MISSING\t\($name)"
    else ($n | types) as $got
      | if $w == "" or ($got | length) == 0 or ($got | index($w)) != null
           or ($w == "integer" and ($got | index("number")) != null)
        then "ok\t\($name)"
        else "TYPE\t\($name)\twant \($w), upstream \($got | join("|"))" end
    end
' "$schema")"

echo "$report" | column -t -s $'\t'

bad="$(echo "$report" | grep -cv '^ok' || true)"
[ "$bad" -eq 0 ] || fail "$bad catalogued codex key(s) missing upstream or retyped (codex $floor catalog)."
echo "OK: every catalogued codex key exists upstream with the expected type."
