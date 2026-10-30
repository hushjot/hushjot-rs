# hushjot-rs

End-to-end encrypted [HushJot](https://hushjot.com) Sync for Rust apps.

Add the crate to a Rust notes app or editor and your users can sign in with
their HushJot account, unlock with their HushJot passphrase, and have their
notes stay in step with the HushJot web app, their phone, the HushJot CLI and
every other HushJot client. Notes are sealed with AES-256-GCM on the device.
The server stores only ciphertext it cannot read.

```toml
[dependencies]
hushjot = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust,no_run
use hushjot::{HushJot, SqliteStore, SyncEvent, SyncOptions, UnlockOptions};

#[tokio::main]
async fn main() -> hushjot::Result<()> {
    let hj = HushJot::builder()
        .store(SqliteStore::open("hushjot.db")?) // ciphertext only
        .build()?;

    let account = hj.sign_in("me@example.com", "account password").await?;
    let vault = account.unlock("my passphrase", UnlockOptions::default()).await?;
    vault.start_sync(SyncOptions::default());

    // Local-first: these return immediately; sync pushes in the background.
    let id = vault.notes().create("# Groceries\n- oat milk", None)?;
    vault.notes().update(&id, "# Groceries\n- oat milk\n- coffee")?;

    let mut events = vault.subscribe();
    while let Ok(event) = events.recv().await {
        if let SyncEvent::Changed { notes, .. } = event {
            println!("{} notes changed on another device", notes.len());
        }
    }
    Ok(())
}
```

## Status

Version 0.1, before 1.0, so the API may still change between minor versions. The
on-disk and wire formats will not change: they are HushJot's, and every client shares
them.

- **Verified:** the test suite on macOS, Linux and Windows in CI, and the Firebase
  emulator journey against the app's live security rules.
- **Minimum Rust:** 1.89.
- **What users need:** a HushJot account with a plan that includes Sync, whose vault
  was created in the HushJot app (see below).

Report bugs in the [issue tracker](https://github.com/hushjot/hushjot-rs/issues), and
security problems as [SECURITY.md](https://github.com/hushjot/hushjot-rs/blob/main/SECURITY.md) describes.

## Try it

The repository has two runnable examples. `mini_editor` is a small terminal note editor
that uses every part of the SDK: sync events, conflicts, folders, search, history,
share links, the recovery code, locking and signing out.

```sh
git clone https://github.com/hushjot/hushjot-rs && cd hushjot-rs
HUSHJOT_EMAIL=you@example.com cargo run -p hushjot --example mini_editor
```

**[docs/mini-editor.md](https://github.com/hushjot/hushjot-rs/blob/main/docs/mini-editor.md)** walks through it command by
command, with the SDK call behind each one.

## What you get

| Area | What the SDK does |
|---|---|
| Accounts | Email and password sign-in, refresh-token resume, and an optional OS-keychain "stay unlocked". |
| Encryption | Argon2id and recovery-code unlock. Every record is bound to its slot. Keys are zeroed on drop and never printed. |
| Notes | Create, edit, pin, move, delete, undelete and list, with titles and excerpts derived exactly as the app derives them. |
| Folders | Nested folders: create, rename, move and delete (with their contents), with cycle-safe trees. |
| Sync | Offline outbox, the app's sync timing and paging, conflict copies (text is never lost), metadata-only rebase, storage and daily-push limits, and maintenance pauses. |
| History | A version per editing session, kept 1 to 30 days, uploaded to the account. List, open and restore. |
| Search | The app's query grammar (`#tag`, `in:Folder`, `"phrase"`, `-not`) over an in-memory index that is never written to disk. |
| Shares | Publish, update, list and revoke links. Each link has its own key, carried in the URL fragment. |

## Crates

- **`hushjot`**: the SDK described above.
- **`hushjot-core`**: the format alone, with no I/O. It covers the envelope, key records, the meta JSON, titles, the folder tree, tags, search, history and share links. It is checked byte for byte against the HushJot app's frozen contract vectors.

## Feature flags

| Flag | Default | What it adds |
|---|---|---|
| `sqlite-store` | on | `SqliteStore`, a durable local store (bundled SQLite). |
| `keyring` | on | `KeyringSecrets`: macOS Keychain, Windows Credential Manager, or Linux Secret Service. |

Without them, use `MemoryStore` and `MemorySecrets`, or implement the `Store` and `SecretStore` traits over your app's own storage.

On Linux, `keyring` uses the Secret Service, so building needs `libdbus-1-dev` and `pkg-config`.

## Before you ship

Read **[docs/integration-guide.md](https://github.com/hushjot/hushjot-rs/blob/main/docs/integration-guide.md)**. It covers:
- the screens an app needs (passphrase, "needs a plan", paused, conflict copies);
- the rules for handling plaintext;
- how to word "Sync by HushJot".

Users need a HushJot account with a plan, from [hushjot.com](https://hushjot.com).
They also need to have opened the HushJot app once to create their vault. Creating a vault issues a recovery code, and this SDK deliberately never does that.

## Tests

The library has about 170 tests:
- **Contract vectors:** bytes sealed by the HushJot app's TypeScript core must open, and every derivation must agree.
- **Property tests:** round-trips, tamper detection, and no panics on arbitrary input.
- **Store conformance:** both stores pass the same checks.
- **Sync scenarios:** a fake server that enforces the real rules (revision compare-and-swap, metering, push window, quota, refusals), with multiple devices racing each other.
- **Emulator journey:** the real REST client runs against the app's live `firestore.rules`. It includes a cross-client check: this SDK writes and the TypeScript CLI reads, and the other way round.

```sh
cargo test --all-features        # everything except the emulator journey
./tools/emulator-test.sh         # the journey (needs the firebase CLI and Java)
```

## Licence

Dual-licensed under [MIT](https://github.com/hushjot/hushjot-rs/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/hushjot/hushjot-rs/blob/main/LICENSE-APACHE), at your option.

The HushJot app and CLI are AGPL-3.0. This SDK is permissively licensed on purpose, so closed-source apps can offer HushJot Sync.
