# Security policy

## Reporting a vulnerability

Please do not open a public issue. Report it privately, either:

- through GitHub: **Security → Report a vulnerability** on this repository, or
- by email to support@hushjot.com, with "Security" in the subject.

Include what you found, how to reproduce it, and what an attacker could do
with it. You will get a reply within a few days.

## Scope

**In scope:**
- both crates in this repository (`hushjot`, `hushjot-core`);
- a format or protocol behaviour that differs from the HushJot app's in a way that weakens it.

**Out of scope:**
- **Apps built on this SDK.** Report those to their authors. The SDK cannot stop an embedding app from logging or uploading the plaintext it is handed. The [integration guide](docs/integration-guide.md) states the rules an app must follow.
- **The public Firebase web config** in `config.rs`. It names the project and grants nothing.

## What the SDK promises

- **Ciphertext only, everywhere it writes.** Only AES-256-GCM ciphertext leaves the device or reaches the local store. The search index exists only in memory.
- **No guessing oracle.** Wrong keys, tampered bytes and records moved between slots fail identically.
- **Key hygiene.**
  - Keys are zeroed on drop and never appear in `Debug` output.
  - Tokens and share keys are redacted from `Debug` too.
- **No account-level writes.** It never creates or rewrites the account's key record, and never hard-deletes anything.
- **No silent data loss.** A conflict is never resolved by discarding text.

## What it does not promise

- **Against a compromised device, nothing helps.** Someone already in the user's unlocked session can read the notes, and a key cached with "stay unlocked" too.
- **The server sees metadata.** The service can see note and folder ids, sizes, timestamps, revision counts and write times. It cannot see titles, text or folder names.
