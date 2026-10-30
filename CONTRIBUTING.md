# Contributing

Thanks for helping. Two rules matter more than any style point.

## 1. The format is not ours to change

`hushjot-core` is a port of the HushJot app's `src/core/`. Every byte it writes is read by the web app, the phone and the CLI. So:

- **Never edit `contract/`.** Those files are vendored from the app repository.
  - Re-vendor them with `tools/sync-contract.sh`.
  - If a contract test fails, the port is wrong.
- **Port line for line.** When you port a derivation (titles, excerpts, tokens, tags, the query grammar), keep JavaScript string semantics: UTF-16 lengths and JS whitespace, through `hushjot_core::js`.
  - Cite the app file the code comes from in the module comment.
- **Where the app changes, this repo follows.** A behaviour change belongs in the app first, with a contract vector. A new `FORMAT_VERSION` means a coordinated release of every client.

## 2. Never lose text

Sync code must preserve both sides of a conflict, never assume a server revision, and never write plaintext to the store. If a change touches `engine.rs`, add a scenario to `crates/hushjot/tests/sync.rs` that would have failed without it.

## Workflow

```sh
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
./tools/emulator-test.sh          # when touching firestore/ or engine.rs (firebase CLI + Java)
```

On Linux, install `libdbus-1-dev` and `pkg-config` first; the `keyring` feature needs them.

### The app and CLI checkouts

Two checks need the other HushJot repositories cloned beside this one:

```text
hushjot-app/   https://github.com/hushjot/hushjot-app
hushjot-cli/   https://github.com/hushjot/hushjot-cli
hushjot-rs/    this repository
```

- **Contract drift:** `tools/sync-contract.sh --check ../hushjot-app`. Without `--check`, it re-vendors.
- **Cross-client emulator step:** the emulator journey also has the TypeScript CLI read what this SDK wrote, and the other way round. It looks for `../hushjot-cli`; set `HUSHJOT_CLI_DIR` if yours is elsewhere. Run `npm install` there first. Without the CLI, the step is skipped and says so.

## Style

- Module comments explain *why*: the trade-off, the bug the shape prevents, and the app file it mirrors. Match that.
- Every public item has rustdoc (`#![deny(missing_docs)]`).
- Commit subjects are short and imperative ("Add share revoke").
- PRs say what behaviour changed and how it was validated.
