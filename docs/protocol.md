# The HushJot Sync protocol, as this crate speaks it

This document is for contributors to `hushjot-rs` and for anyone implementing the `Backend` trait.

The authorities are in the HushJot app repository: `firestore.rules`, `docs/security.md` and `src/core/`. This document summarises them and says where this crate implements each rule. If this document and the app disagree, the app is right; fix this document.

## Key hierarchy

```text
DEK  = 32 random bytes                                       (never derived)
passphrase ──Argon2id(params, salt_p from the record)──► KEK_p ─ v1 seal(DEK) = wrappedByPassphrase
recovery   ──HKDF-SHA256(salt_r, info "teksto/recovery-kek/v1")► KEK_r ─ v1 seal(DEK) = wrappedByRecovery
```

- **Where it lives:** `users/{uid}/keys/primary` = `{v: 1, record: SerializedKeyRecord, updatedAt}`. A reset account has a tombstone there: `{resetAt, …}` with no `record`.
- **No verifier.** A failed GCM tag on unwrap is the wrong-secret signal.
- **Recovery-code normalisation for the KDF** only strips whitespace and `-` and uppercases. The confusable mapping (O→0 and so on) is for display and validation only.
- **This crate never writes `keys/primary`.**
- **Code:** `hushjot_core::{kdf, key_record}`.

## Envelope

| Bytes | Content |
|---|---|
| `[0]` | version: 1 = unbound, 2 = bound |
| `[1]` | algorithm: 1 = AES-256-GCM |
| `[2..14)` | 12-byte random nonce |
| `[14..]` | ciphertext followed by the 16-byte tag |

- **v1:** the AAD is the 2-byte header.
- **v2:** the AAD is the header followed by the UTF-8 context `hushjot:<kind>:<id>`, where kind is `note-meta`, `note-body`, `folder` or `share-key`.
- **What is v2:** everything sealed under the DEK.
- **What stays v1:** the key-record wraps and share payloads.
- **Legacy:** v1 is accepted where a context is expected while `ACCEPT_UNBOUND_LEGACY` holds.
- **Code:** `hushjot_core::envelope`.

## Plaintexts

| Record | Plaintext |
|---|---|
| Note meta | `{"title","folderId"[,"pinnedAt"][,"excerpt"][,"createdAt"]}`: fixed order, optional fields omitted when absent, `createdAt: 0` kept |
| Note body | the Markdown as UTF-8 |
| Folder | `{"name","parentId"}` |
| Share payload | `{"v":1,"title","body","updatedAt"}` under the share's own key |

- **Title** = `derive_title(body)`, cut at 120 UTF-16 units. The one exception is a conflict copy, titled `"<title> (conflict copy YYYY-MM-DD HH:MM)"` (UTC).
- **Excerpt** = `excerpt_of(body)`, cut at 100 UTF-16 units.

## Documents

| Path | Fields |
|---|---|
| `users/{uid}/notes/{id}` | `v=1`, `meta: bytes`, `body?: bytes`, `revision: int`, `updatedAt: int`, `deleted: bool`, `syncedAt = REQUEST_TIME` |
| `users/{uid}/folders/{id}` | `v=1`, `data: bytes` (under 16 KiB), `createdAt`, `updatedAt`, `deleted`, `syncedAt = REQUEST_TIME` |
| `users/{uid}/notes/{id}/versions/{sealId}` | `v`, `meta`, `body`, `at`, `reason` (edit, delete or restore), `expireAt` (at most 31 days ahead), `createdAt = REQUEST_TIME`. Create-only. |
| `users/{uid}/shares/{shareId}` | `v`, `noteId`, `key` (the share key sealed v2 `share-key`), `expireAt`, `createdAt = REQUEST_TIME` |
| `shares/{shareId}` | `v`, `payload`, `expireAt` (equal to the index entry's), `createdAt = REQUEST_TIME`. Readable by anyone until expiry. |
| `users/{uid}/usage/current` | the meter: `bytes`, `pn`, `ws`, `pushes.{YYYYMM}`, `hist.{YYYYMM}`, `shares.{YYYYMM}`, `lastDoc` |

Ids match `[A-Za-z0-9_-]{1,64}`. A `sealId` is the body's nonce as 24 hex characters. A share id is 22 characters of base64url.

## Write rules

1. **`syncedAt` is a server transform.** Writes go through `:commit` with `updateTransforms`.
2. **Full replace, no `updateMask`,** except for the meter, which is a masked merge.
3. **Ciphertext is `bytesValue`** in standard padded base64.
4. **Note compare-and-swap.** An update must carry `revision == stored + 1`.
   - The server revision is not the local one.
   - The engine keeps `sync:note:<id>` = `{base, seen?, bodySeal?, baseBytes?}`.
   - Never assume a revision: a note with no base reads the document first.
5. **Every write carries the meter in the same commit.** The rules check four things:
   - **`bytes`** moves by `charged(new) − charged(old)`, where `charged(n) = n ≤ 0 ? 0 : max(n, 1024)` and size is meta plus body (or a folder's `data`).
   - **`pn`** goes up by 1, or resets (`ws = REQUEST_TIME`, `pn = 1`) once the window is over 24 h old by the server clock.
   - **`hist`** goes up by a version's charge.
   - **`shares`** goes up by 1 per publish.

   `lastDoc` names the document changed.
6. **No deletes,** except share documents: the public one first, then the private one. A note delete is a tombstone: meta kept, no body, revision + 1.
7. **A 403 is ambiguous.**
   - **On a read:** probe `GET status/lockdown`. Allowed (404 or 200) means paused; refused means not approved.
   - **On a write:** read the document.
     - If the read is refused, the account is not approved.
     - If it shows a different revision, it is a conflict.
     - Otherwise the refusal was about the meter. Retry once with the size actually stored, or as a window reset; else report storage full or the push limit.

## Pull

Run one query per collection, ordered by `(syncedAt, __name__)`, 200 to a page.

- **Cursor:** one position `{at, id}` per collection. It resumes with `startAt [ts, ref] before:false`, advances to the page's last document, and never moves backwards.
- **Applying a remote note:** only when its revision is above the recorded base and the note is not queued. If it is queued, record `seen`.
- **Folders:** last write wins on `updatedAt` (strictly newer), and a tombstone always wins.

## Conflicts

When a push is refused, read the document again.

| What the read shows | What happens |
|---|---|
| Revision unchanged | Not a conflict: park the note. |
| Revision base + 1, same `updatedAt`, **same bytes** | Our own lost acknowledgement: adopt it. |
| Our stored body's nonce still equals the recorded `bodySeal` | Metadata-only change: adopt the remote, lay the local pin and folder over it, and re-push. |
| Anything else | The remote keeps the id, and the local text becomes a conflict copy. Identical texts make no copy. |

Two consecutive conflicts park the note.

## Differences from the web app

- **Same-millisecond writes.** The lost-ack check also compares bytes, and local folder stamps are strictly increasing, so two devices writing in the same millisecond cannot mistake each other's write for their own.
- **No listener.** Over REST the idle backstop (60 s) pulls. Apps call `sync_now()` on focus.
- **One process, one engine.** There is no multi-tab leader election.
