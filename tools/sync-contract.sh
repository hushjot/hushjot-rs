#!/usr/bin/env bash
# Re-vendor the format contract from the HushJot app repository, or with
# --check, fail if the vendored copy has drifted (CI runs this).
#
#   tools/sync-contract.sh [--check] [path/to/hushjot-app]
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
check=0
if [[ "${1:-}" == "--check" ]]; then check=1; shift; fi
app="${1:-$root/../hushjot}"
files=("contract/format-vectors.json:contract/format-vectors.json" "contract/behaviour-vectors.json:contract/behaviour-vectors.json" "firestore.rules:contract/firestore.rules")
status=0
for pair in "${files[@]}"; do
  src="$app/${pair%%:*}"; dst="$root/${pair##*:}"
  if [[ $check == 1 ]]; then
    if ! cmp -s "$src" "$dst"; then echo "drift: ${pair##*:} differs from $src"; status=1; fi
  else
    cp "$src" "$dst"; echo "vendored ${pair##*:}"
  fi
done
exit $status
