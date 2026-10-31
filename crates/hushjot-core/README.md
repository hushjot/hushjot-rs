# hushjot-core

The [HushJot](https://hushjot.com) note format, with no I/O. It covers:
- the AES-256-GCM envelope and record binding;
- key records and Argon2id/HKDF unlock;
- the sealed JSON shapes;
- title and excerpt derivation, the folder tree and inline `#tags`;
- the search grammar and index;
- metering arithmetic, history retention and share links.

Every function here must agree byte for byte with the HushJot app's TypeScript core, so the tests run the app's frozen contract vectors.

Most apps want the [`hushjot`](https://crates.io/crates/hushjot) crate, which adds sign-in, sync and a local store. Use this crate directly to work with the format alone.

```rust
use hushjot_core::envelope::{open_text, record_context, seal_text, Key, RecordKind};
use hushjot_core::notes::derive_title;

let key = Key::generate();
let ctx = record_context(RecordKind::NoteBody, "note-1");
let sealed = seal_text(&key, "# Hello\nworld", Some(&ctx));
assert_eq!(open_text(&key, &sealed, Some(&ctx)).unwrap(), "# Hello\nworld");
assert_eq!(derive_title("# Hello\nworld"), "Hello");
```

Copyright © 2026 Nebulosa d'Ideias Unipessoal Lda.

Dual-licensed under [MIT](https://github.com/hushjot/hushjot-rs/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/hushjot/hushjot-rs/blob/main/LICENSE-APACHE), at your option. Source, issues and security reporting: [github.com/hushjot/hushjot-rs](https://github.com/hushjot/hushjot-rs).
