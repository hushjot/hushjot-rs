# Integrating HushJot Sync

This guide is for teams building an app on the `hushjot` crate. It covers:
- what your users need;
- the screens to build;
- what to show for each sync state;
- the handful of rules that keep HushJot's end-to-end encryption true inside your app.

## What your users need

1. **A HushJot account with a plan.** Accounts and plans are bought at
   [hushjot.com](https://hushjot.com). The SDK cannot create an account or take payment.
2. **A vault.** The first time anyone opens the HushJot app, it creates the vault and shows a recovery code once.
   - A brand-new account that has never opened the app gets `Error::NoVault`.
   - Tell the user: "Open HushJot once at hushjot.app to set up your passphrase, then come back."

## The flow

```text
launch
 ├─ hj.resume(email)  → Some(account)                     (saved session)
 │                    → None → sign-in screen → hj.sign_in(email, password)
 ├─ account.unlock_cached() → Some(vault)                 (user chose "stay unlocked")
 │                          → None → passphrase screen → account.unlock(passphrase, options)
 └─ vault.start_sync(SyncOptions::default())
```

This works offline too, once the account has been online on this device: `resume` rebuilds the saved session, `unlock_cached` (or `unlock`) uses the last key record it fetched, and sync shows Offline until the network returns.

Keep the `HushJot` value and the `Vault` for the life of the window. `Vault` is cheap to clone, and every clone is the same vault.

### The sign-in screen

Ask for the **email** and the **account password**. The account password is not the passphrase. Label the two fields differently, because users confuse them.

| Error | Show |
|---|---|
| `SignIn(msg)` | `msg`. It is safe to show and never says which half was wrong. |
| `Transient` | "Couldn't reach HushJot. Check your connection." |

Link "Don't have an account?" to `https://hushjot.com`.

### The passphrase screen

Ask for the **HushJot passphrase**. Offer two more things on this screen:
- a "Use recovery code" path (`unlock_with_recovery_code`);
- a "Stay unlocked on this device" checkbox (`UnlockOptions { stay_unlocked: true }`).

| Error | Show |
|---|---|
| `WrongSecret` | "That passphrase isn't right." Let them retry. |
| `NoVault` | "Set up HushJot once at hushjot.app first." |
| `VaultReset` | See [Reset accounts](#reset-accounts). |
| `NotApproved` | "This account doesn't include HushJot Sync." Link to `hushjot::PLANS_URL`. |
| `Paused` | "HushJot Sync is down for maintenance. Try again later." |

Unlocking runs Argon2id. That takes about 100 ms on a laptop and runs on a blocking thread, so show a spinner rather than freezing.

**"Stay unlocked"** stores the raw vault key in the OS keychain:
- **What it protects against:** another user account on the machine, and a stolen disk.
- **What it does not:** someone already using this user's unlocked session.

Say that in the checkbox's help text. Leave it off by default.

## Editing

```rust
let id = vault.notes().create(&text, folder_id)?;  // returns at once
vault.notes().update(&id, &text)?;                 // call on your editor's save debounce
```

- **Writes are local-first.** They seal, store, queue and return without waiting for the network, so call them from your save path freely.
- **Debounce your saves.** A few hundred milliseconds of idle is plenty. History keeps one version per editing session, not one per save, so frequent saves cost nothing there.
- **Save with `update_if_unchanged` when unsaved text can meet a pull.** Pass the text your editor last read or saved as `expected`. If another device's edit (or delete) arrived meanwhile you get `ConditionalUpdate::Changed` (or `Deleted`) and nothing is written: keep the user's text some other way (a new note is simplest) rather than calling `update`, which writes over whatever the note holds. On `SyncEvent::ConflictCopy`, your text is in the copy already: move the open editor to `copy_id`, with the copy's text as `expected`, so the next save doesn't make a second copy.
- **The title is the first line.** Notes have no separate title field. Show `summary.title` and do not offer a title box.
- **Pinning and moving** are separate calls (`set_pinned`, `move_to`), never a re-save of the text.
- **Deleting is recoverable.** `notes().recently_deleted()` and `notes().undelete(id)` give you a Trash view.
- **Deleting a folder deletes its contents.** Warn the user first with `folders().contents(id)`, which returns the subfolder and note counts.

## While syncing

Subscribe once and update your UI from the events:

```rust
let mut events = vault.subscribe();
while let Ok(event) = events.recv().await { … }
```

| Event | What to do |
|---|---|
| `Changed { notes, folders }` | Reload those from the vault. **If one of the changed notes is open and the user has typed since your last save, do not replace their text.** Their next save will sync as an edit, and a real conflict becomes a copy. |
| `ConflictCopy { note_id, copy_id }` | Tell the user something like "Edited on two devices. Your version was kept as a copy." Offer to open both. |
| `Status(SyncStatus::Offline)` | Show a quiet "Offline". Changes stay queued. |
| `StorageFull { waiting }` | "Storage full. N changes waiting." Changes go once space frees. |
| `PushLimit { waiting, retry_at }` | "Daily sync limit reached. Syncing resumes at …". |
| `NoteTooLarge { note_id }` | "This note is too large to sync (about 900 KB limit)." |
| `SessionExpired` | Go to the sign-in screen. The vault stays usable meanwhile. |
| `NotApproved` | "Sync isn't included in this account." Link to `PLANS_URL`. Keep the editor working. |
| `Paused` | "HushJot Sync is paused for maintenance." **Never** suggest buying a plan here. |
| `Pending(n)` | Optional "n unsynced" indicator. |

The engine pulls every 60 seconds when idle. Call `vault.sync_now().await` when your window gains focus, so changes from the phone appear at once.

**The editor must never be disabled because sync is unhappy.** Every state above leaves the notes fully editable, with changes queued.

## Rules for plaintext

HushJot's promise is that only the user's devices can read their notes. Inside your app, keeping that promise is your responsibility:

1. **Never log note text, titles, folder names, passphrases, passwords or tokens.**
   - This covers debug logs, crash reporters, analytics and telemetry.
   - The SDK's own `Debug` output redacts keys and tokens, but `Note.content` is plain data.
2. **Never write plaintext to disk.** That rules out:
   - temp files;
   - swap or backup files of an open note;
   - autosave outside the vault;
   - search indexes.

   The `SqliteStore` holds only ciphertext, and the search index lives in memory. Keep it that way.
3. **Never send note content to a server.** That includes your own server, an AI service, or a link previewer. If you add such a feature, make it explicit and opt-in, and say plainly what leaves the device.
4. **Lock on request.** Offer "Lock". Call `vault.lock()` and drop your copies of open text.
5. **Do not paste user secrets into issue reports.** The SDK's errors never contain them, so an error string is safe to report.

## Reset accounts

A user who lost both their passphrase and recovery code can reset their account in the HushJot app. That makes a new, empty vault. Afterwards, a device whose local store holds the old vault's ciphertext gets `Error::VaultReset` on unlock. That data can never be pushed, and could never be read by the new vault.

Explain this to the user and call `hj.wipe_local_data()`, then sign in again. If you want the old notes, export them first, while you still hold an unlocked vault on the old key.

## Several accounts

One `Store` belongs to one account; unlock refuses a store bound to another account. For multiple accounts, give each its own `SqliteStore` file.

## Naming and branding

- **Use:** "Sync by HushJot", or "HushJot Sync", and link to hushjot.com.
- **Do not:** imply your app *is* HushJot, use HushJot's logo as your app's icon, or describe the encryption differently from HushJot's own security documentation.
- **Ask first:** if you want co-marketing, ask.

## Support boundaries

- **Questions about accounts, plans, billing and the HushJot app** go to HushJot support.
- **Questions about your app** go to you.
- **SDK bugs** go to the hushjot-rs issue tracker. See `SECURITY.md` for vulnerabilities.
