# Try the SDK: the mini editor

`mini_editor` is a small terminal note editor built on the `hushjot` crate
([source](../crates/hushjot/examples/mini_editor.rs), about 350 lines). It uses every
part of the SDK an app needs:
- sign-in, the saved session, unlocking and "stay unlocked";
- notes, folders, search and tags;
- history and share links;
- background sync and its events, conflict copies and working offline;
- the recovery code, locking, signing out and a reset account.

This guide walks through it command by command. Each step names the SDK call behind the
command, so you can find the same thing in the source and in the
[API docs](https://docs.rs/hushjot). Read it alongside the
[integration guide](integration-guide.md), which says what a real app should show for
each state.

> **Use a test account.** The example talks to the real HushJot service and changes real
> notes. Start with an account whose notes you don't mind changing.

---

## 1. What you need

- **Rust 1.89 or newer:** check with `rustc --version`.
- **On Linux:** `libdbus-1-dev` and `pkg-config`, for the Secret Service keychain. You
  also need a running keyring daemon, such as GNOME Keyring or KWallet.
  ```sh
  sudo apt-get install libdbus-1-dev pkg-config
  ```
- **A HushJot account with a plan that includes Sync,** from
  [hushjot.com](https://hushjot.com).
- **A vault.** Open the account once in the HushJot app at
  [hushjot.app](https://hushjot.app). The app creates the vault, sets your passphrase
  and shows a recovery code. The SDK deliberately never creates a vault.
  - Write the recovery code down; one step below uses it.
- **Optional:** the HushJot app open on your phone or in a browser, so you can watch
  changes arrive in both directions. `sqlite3`, for step 10.

HushJot has two secrets, and the editor asks for both:

| Secret | What it does | SDK call |
|---|---|---|
| **Account password** | Signs in to the service. | `HushJot::sign_in` |
| **Passphrase** | Unlocks the vault key. The server never sees it. | `Account::unlock` |

---

## 2. Run it

```sh
git clone https://github.com/hushjot/hushjot-rs
cd hushjot-rs
HUSHJOT_EMAIL=you@example.com cargo run -p hushjot --example mini_editor
```

The first build takes a minute (SQLite is compiled in).

- **Prompts:** the editor reads the password and passphrase from the terminal, and
  **echoes them as you type**. That keeps the example short; a real app uses a password
  field.
- **Leaving out `HUSHJOT_EMAIL`:** the editor asks for the email instead.

### Where it keeps things

| What | Where | Holds |
|---|---|---|
| Local store | `store.db` in a temp folder (`$TMPDIR/hushjot-mini-editor/` on macOS, `/tmp/hushjot-mini-editor/` on Linux, `%TEMP%\hushjot-mini-editor\` on Windows). The editor prints the path at start-up. | Ciphertext only: `SqliteStore`. |
| Session and cached key | The OS keychain, service `com.hushjot.examples.mini-editor`: Keychain Access on macOS, Credential Manager on Windows, the Secret Service on Linux. | `<email>:refresh`, and with "stay unlocked" also `<email>:dek-name` and `<email>:dek:…`. This is `KeyringSecrets`. |

Set `HUSHJOT_DATA_DIR=/some/folder` to use a different store. You need this for a
second copy of the editor (step 9) or a second account (step 11).

---

## 3. Sign in and unlock

**First launch**

1. Type the **account password**: `hj.sign_in(email, password)`.
   - A wrong password gives a sign-in error that doesn't say which half was wrong
     (`Error::SignIn`).
2. Type the **passphrase**: `account.unlock(passphrase, options)`.
   - A wrong one prints "That passphrase is not right." and asks again
     (`Error::WrongSecret`).
3. At **"Stay unlocked on this device? [y/N]"**, answer `n` this time.
4. It prints `Unlocked you@example.com. Type help.`

Behind the scenes, `vault.subscribe()` starts listening for sync events, and
`vault.start_sync(SyncOptions::default())` starts the background engine.

**Second launch**

5. Type `quit`, then run the editor again.
   - It doesn't ask for the password: `hj.resume(email)` rebuilt the session from the
     keychain.
   - It does ask for the passphrase, because you said no to staying unlocked.
6. This time answer `y`. Then `quit` and run it again.
   - It asks for nothing: `account.unlock_cached()` found the vault key in the keychain.
   - Look in your keychain for `com.hushjot.examples.mini-editor` to see the entries.

**Offline:** turn off Wi-Fi and launch again. It still unlocks and `ls` still works. Once
a device has been online, `resume` and unlocking work offline, and sync reports
`Offline`.

---

## 4. Notes

Type `help` at any time for the full list.

| Command | What it does | SDK call |
|---|---|---|
| `ls` | Lists notes, pinned first (`*`), with an 8-character id. | `vault.notes().list()` |
| `new <text>` | Creates a note. Write `\n` for a line break. Prints the full id. | `vault.notes().create(text, None)` |
| `cat <id>` | Prints a note. | `vault.notes().get(id)` |
| `append <id> <text>` | Adds a line. | `get`, then `vault.notes().update(id, text)` |
| `pin <id>`, `unpin <id>` | Pins or unpins. | `vault.notes().set_pinned(id, bool)` |
| `rm <id>` | Deletes. It can be undone. | `vault.notes().delete(id)` |
| `trash` | Lists recently deleted notes, with full ids. | `vault.notes().recently_deleted()` |
| `undelete <full id>` | Brings a deleted note back. | `vault.notes().undelete(id).await` |
| `mv <id> <folder id>`, `mv <id> /` | Moves a note into a folder, or back to the top level. | `vault.notes().move_to(id, folder)` |
| `big <kb>` | Creates a note of about that many KB, to test the size limit. | `create` |

Where a command takes `<id>`, the prefix from `ls` is enough. `undelete` needs the full
id, which `new` and `trash` print.

**Try this**

- `new # Rust test\nfirst line #sdktest`. The note shows up in the HushJot app titled
  "Rust test".
  - Notes have no title field: the title is derived from the first line, exactly as the
    app derives it.
  - Writes are local-first. `create` returns at once, and the engine pushes in the
    background about 100 ms later.
- `new # Ünïcödé 🎉\nça va`. The title shows the same in the app.
- Create or edit a note in the app. Within 60 seconds, or straight after `sync`, the
  editor prints `[sync] 1 note(s) changed` and `ls` shows the change.
- `rm` a note, check `trash` against the app's recently deleted list, then `undelete` it.

---

## 5. Folders

| Command | What it does | SDK call |
|---|---|---|
| `folders` | Shows the folder tree with 8-character ids. | `vault.folders().tree()` |
| `mkdir <name>` | Creates a top-level folder. | `vault.folders().create(name, None)` |
| `rename-folder <id> <name>` | Renames a folder. | `vault.folders().rename(id, name)` |
| `mvdir <id> <parent id>`, `mvdir <id> /` | Moves a folder into another one, or to the top level. | `vault.folders().move_to(id, parent)` |
| `rmdir <id>` | Asks first, then deletes the folder **and everything in it**. | `vault.folders().contents(id)`, then `vault.folders().delete(id)` |

**Try this**

- `mkdir SDK Test`, `mkdir Inner`, then `mvdir <Inner> <SDK Test>`. The app shows Inner
  inside SDK Test.
- `mvdir <SDK Test> <Inner>`. This is **refused**: a folder can't go inside its own
  subfolder.
- `mv <note> <Inner>`, then `rmdir <SDK Test>`. The prompt counts 1 subfolder and 1 note,
  using `contents()`. A real app should show the same warning. The note goes to `trash`.

---

## 6. Search and tags

| Command | What it does | SDK call |
|---|---|---|
| `search <query>` | The top 20 matches, with scores. | `vault.search(query, 20)` |
| `tags` | Inline `#tags` with counts. | `vault.tags()` |

The query grammar is the app's own:

| Query | Matches |
|---|---|
| `#sdktest` | notes tagged `#sdktest` |
| `in:"SDK Test"` or `in:SDK` | notes in that folder |
| `"first line"` | the exact phrase |
| `#sdktest -second` | tagged notes that don't contain "second" |

The search index is built in memory when you unlock and is never written to disk. Run the
same queries in the app: the results match.

---

## 7. History

| Command | What it does | SDK call |
|---|---|---|
| `history <id>` | Lists versions. Ones fetched from the account are marked `(server)`. | `vault.history(id).list().await` |
| `restore <id> <version id>` | Makes an old version the note's text again. The current text is saved as a version first, so a restore can be undone. | `vault.history(id).restore(version).await` |

History keeps one version per **editing session**, not one per save. To see several
versions, `append` to a note, wait a few minutes, and `append` again. Versions are sealed
and uploaded, so the app's history view shows the ones the editor made.

---

## 8. Share links

| Command | What it does | SDK call |
|---|---|---|
| `share <id>` | Publishes a link that expires after one day. Prints the URL. | `vault.shares().publish(id, ShareExpiry::Day).await` |
| `shares` | Lists live links. | `vault.shares().list().await` |
| `reshare <share id>` | Re-publishes the link with the note's current text. The URL stays the same. | `vault.shares().update(id, ShareExpiry::Day).await` |
| `unshare <share id>` | Revokes the link. | `vault.shares().revoke(id).await` |

Each link has its own key, and the key is in the URL **fragment** (after `#`). Browsers
don't send the fragment to the server.
- Open the link in a private window: you see the note.
- Delete everything after `#`: the note no longer opens.

---

## 9. Sync, conflicts and offline

| Command | What it does | SDK call |
|---|---|---|
| `sync` | Syncs now and prints the resulting status. | `vault.sync_now().await` |
| `usage` | Storage used and the account's limits. | `vault.usage().await` |

The engine pushes about 100 ms after each change. When it's idle, it pulls every 60
seconds. A real app should also call `sync_now()` when its window gains focus.

### What the editor prints

These are the `SyncEvent`s the editor shows. The integration guide says what a real app
should do with each one.

| Printed | Event | Meaning |
|---|---|---|
| `[sync] N note(s) changed` | `Changed { notes, .. }` | Another device changed these notes. Reload them, but never overwrite text the user is typing. |
| `[sync] conflict kept as a copy: <id>` | `ConflictCopy { .. }` | Both sides edited the same note. Both versions were kept. |
| `[sync] offline; changes stay queued` | `Status(SyncStatus::Offline)` | No network. Edits are queued in the store. |
| `[sync] note … is too large to sync` | `NoteTooLarge { .. }` | The note is over about 900 KB. It stays on this device only. |
| `[sync] storage full; N waiting` | `StorageFull { .. }` | The account is full. Changes wait. |
| `[sync] daily limit; N waiting` | `PushLimit { .. }` | The daily push limit was reached. |
| `[sync] this account needs a plan` | `NotApproved` | The account's plan doesn't include Sync. Editing keeps working. |
| `[sync] HushJot Sync is paused for maintenance` | `Paused` | Try again later. |
| `[sync] signed out; restart to sign in` | `SessionExpired` | The session ended. |

### Experiments

**Offline queue**
1. Turn off Wi-Fi.
2. `new # Offline note`, then `sync`. It prints `Offline`, with no crash and no hang.
3. Turn Wi-Fi back on and run `sync`. The note appears in the app.

**A conflict copy (text is never lost)**
1. Turn off Wi-Fi.
2. `append <id> laptop edit`.
3. In the app, edit the same note: add "phone edit".
4. Turn Wi-Fi on and run `sync`. The editor prints `conflict kept as a copy`.
5. Both edits still exist, one in the original note and one in the copy, in the editor
   and in the app.

**Two clients on one machine**

In a second terminal, run:

```sh
HUSHJOT_DATA_DIR=/tmp/hushjot-mini-editor-2 HUSHJOT_EMAIL=you@example.com \
  cargo run -p hushjot --example mini_editor
```

Edit the same note in both, then `sync` in both. Nothing is lost. You may get a conflict
copy.

**The size limit**
1. `big 950`, then `sync`. It prints `too large to sync`, and the other notes still sync.
2. `rm` that note.
3. `big 800` syncs normally. Delete it afterwards.

---

## 10. Check that only ciphertext is on disk

1. `new # zebrapancake secret`, then `quit`.
2. Search the store for that word, using the store path the editor printed at start-up:
   ```sh
   sqlite3 "$TMPDIR/hushjot-mini-editor/store.db" .dump | grep -i zebrapancake
   grep -ri zebrapancake "$TMPDIR/hushjot-mini-editor"
   ```
3. Both print **nothing**. Try your passphrase and password too: also nothing.

Every note body, title, folder name and history version is sealed with AES-256-GCM
before it reaches the store or the network. Plaintext exists only in memory, and only
while the vault is unlocked.

---

## 11. Locking, signing out and recovery

| Command | What it does | SDK call |
|---|---|---|
| `lock` | Drops the key from memory and asks for the passphrase again. | `vault.lock()` |
| `forget-key` | Removes the "stay unlocked" key from the keychain. | `account.forget_cached_key().await` |
| `signout` | Removes the session and the cached key, then quits. | `account.sign_out()` |
| `quit` | Syncs, locks and exits. | `vault.sync_now().await`, then `vault.lock()` |

**Recovery code**
1. Run `forget-key`, then `quit`, then run the editor again.
2. At the passphrase prompt, type `recovery`, then your recovery code:
   `account.unlock_with_recovery_code(code, options)`.
   - A wrong code prints "That recovery code is not right."

**Other errors you can trigger**

| Situation | Message | Error |
|---|---|---|
| An account that has never opened the app | "Set up HushJot once at hushjot.app first" | `NoVault` |
| An account without Sync | "This account doesn't include HushJot Sync" | `NotApproved` |
| Two accounts with one store: run with a second account's `HUSHJOT_EMAIL` and no `HUSHJOT_DATA_DIR` | "This store belongs to another account" | `StoreBelongsToAnotherAccount` |
| The account was reset in the app, then you unlock with the new passphrase | "This account was reset", with an offer to wipe this device's copy (`hj.wipe_local_data()`) | `VaultReset` |
| Service maintenance | "HushJot Sync is down for maintenance" | `Paused` |

---

## 12. Troubleshooting

- **Linux build fails on `dbus` or `pkg-config`.** Install `libdbus-1-dev` and
  `pkg-config`.
- **Linux: "stay unlocked" fails, or it asks every time.** No Secret Service is running.
  Start GNOME Keyring or KWallet. In a headless session, answer `n` to "stay unlocked".
- **"Set up HushJot once at hushjot.app first".** Open the account in the app once.
- **"This store belongs to another account".** Set `HUSHJOT_DATA_DIR` to a fresh folder.
- **Changes from the app don't appear.** Run `sync`. If `usage` fails, check that your
  plan includes Sync.

---

## 13. Clean up

1. Delete the test notes, folders and links, in the app or with `rm`, `rmdir` and
   `unshare`.
2. Run `signout`. That removes the keychain entries.
3. Delete the local stores:
   ```sh
   rm -rf "$TMPDIR"/hushjot-mini-editor* /tmp/hushjot-mini-editor-2
   ```
4. If you put a password in a command line, clear it from your shell history.

---

## From the example to your app

The editor's structure is the one the integration guide recommends:

- **Start-up:** `resume` or `sign_in`, then `unlock_cached` or `unlock`, then
  `start_sync`.
- **Events:** `subscribe()` once, and update the UI from `SyncEvent`s.
- **Edits:** call `notes().update()` on every save. It's local and returns at once.
- **Errors that end the session:** `explain()` in the source shows what to say for each.

Before you ship, read the [integration guide](integration-guide.md), especially
**Rules for plaintext** and **Naming and branding**.
