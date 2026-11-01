# Changelog

## Unreleased

- **`hushjot`:**
  - `Notes::update_if_unchanged` writes a note's text only while it still holds the text the editor last read, as one step with the sync, and returns `ConditionalUpdate::Changed` (or `Deleted`) otherwise. An editor holding unsaved text while another device's edit arrives no longer has to choose between writing over it and losing its own.
  - Fixed: a pull that landed between a local write's read and its write (an edit, pin, move, delete, restore, undelete or a folder's delete) was overwritten unseen. Those writes, and the sync's application of a pulled note and settling of a conflict, now hold one write guard; the vault's write helpers take only rows read under it.

## 0.1.0 (unreleased)

First release. The emulator journey runs against the app's live rules.

- **`hushjot-core`:**
  - Envelope v1 and v2, key records, Argon2id and HKDF unlock.
  - Note meta, folder and share JSON.
  - Titles, excerpts and list order.
  - The folder tree and tags.
  - The search grammar, index and phrase verification.
  - Metering arithmetic, history policy and share links.
- **`hushjot`:**
  - Sign-in and session refresh, and the Firestore REST protocol.
  - SQLite and memory stores, OS-keychain secrets.
  - A local-first vault: notes, folders, history, shares and search.
  - The sync engine, with conflict copies, metadata rebase, limits and pauses.
  - Offline launch: `resume` and unlocking work without the network once the device has been online.
- **Examples:** `quickstart`, and `mini_editor`, a terminal editor covering the whole SDK (walkthrough in `docs/mini-editor.md`).
