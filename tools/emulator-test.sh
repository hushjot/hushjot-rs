#!/usr/bin/env bash
# Run the emulator journey (crates/hushjot/tests/emulator.rs) against the
# vendored contract/firestore.rules. Needs the `firebase` CLI and Java.
#
# Ports 9097/8087, so neither the app's emulators (9098/8085) nor the CLI's
# journey (9099/8086) collide with this one.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$root/contract/firestore.rules" "$work/firestore.rules"
cat > "$work/firebase.json" <<JSON
{
  "firestore": { "rules": "firestore.rules" },
  "emulators": {
    "auth": { "port": 9097 },
    "firestore": { "port": 8087 },
    "ui": { "enabled": false },
    "singleProjectMode": true
  }
}
JSON
# Build first so the emulators are not kept waiting on the compiler.
(cd "$root" && cargo test -p hushjot --test emulator --no-run)
cd "$work"
firebase emulators:exec --only firestore,auth --project teksto-dev \
  "cd '$root' && HUSHJOT_EMULATOR=1 cargo test -p hushjot --test emulator -- --nocapture"
