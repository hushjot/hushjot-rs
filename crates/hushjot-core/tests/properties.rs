//! Property tests: invariants that must hold for every input, not just the
//! cases someone thought to write down.

use hushjot_core::envelope::{open, open_text, record_context, seal, seal_text, Key, RecordKind};
use hushjot_core::js::utf16_len;
use hushjot_core::meta::{note_meta_json, parse_note_meta, NoteMetaPayload};
use hushjot_core::notes::{derive_title, excerpt_of, MAX_EXCERPT, MAX_TITLE};
use hushjot_core::search::{parse_query, tokenize, IndexDocument, SearchIndex};
use hushjot_core::tags::extract_tags;
use proptest::prelude::*;

fn key() -> Key {
    Key::from_bytes([42; 32])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn seal_then_open_returns_the_plaintext(data in proptest::collection::vec(any::<u8>(), 0..2048), id in "[A-Za-z0-9_-]{1,64}") {
        let ctx = record_context(RecordKind::NoteBody, &id);
        prop_assert_eq!(open(&key(), &seal(&key(), &data, Some(&ctx)), Some(&ctx)).unwrap(), data.clone());
        prop_assert_eq!(open(&key(), &seal(&key(), &data, None), None).unwrap(), data);
    }

    #[test]
    fn text_round_trips(text in any::<String>()) {
        prop_assert_eq!(open_text(&key(), &seal_text(&key(), &text, None), None).unwrap(), text);
    }

    #[test]
    fn a_bound_envelope_never_opens_under_another_id(a in "[a-z0-9]{1,16}", b in "[a-z0-9]{1,16}") {
        prop_assume!(a != b);
        let sealed = seal(&key(), b"x", Some(&record_context(RecordKind::NoteBody, &a)));
        prop_assert!(open(&key(), &sealed, Some(&record_context(RecordKind::NoteBody, &b))).is_err());
    }

    #[test]
    fn opening_arbitrary_bytes_never_panics(data in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = open(&key(), &data, None);
        let _ = open(&key(), &data, Some("hushjot:folder:x"));
    }

    #[test]
    fn meta_json_round_trips(
        title in any::<String>(),
        folder in proptest::option::of("[a-z0-9-]{1,36}"),
        pinned in proptest::option::of(0i64..4_000_000_000_000),
        excerpt in proptest::option::of(".{1,100}"),
        created in proptest::option::of(0i64..4_000_000_000_000),
    ) {
        let payload = NoteMetaPayload { title, folder_id: folder, pinned_at: pinned, excerpt, created_at: created };
        let json = note_meta_json(&payload);
        let back = parse_note_meta(json.as_bytes()).unwrap();
        prop_assert_eq!(note_meta_json(&back), json);
        prop_assert_eq!(back, payload);
    }

    #[test]
    fn titles_and_excerpts_stay_within_their_caps(content in any::<String>()) {
        prop_assert!(utf16_len(&derive_title(&content)) <= MAX_TITLE);
        prop_assert!(!derive_title(&content).is_empty());
        prop_assert!(utf16_len(&excerpt_of(&content)) <= MAX_EXCERPT);
    }

    #[test]
    fn tokens_are_lowercase_and_bounded(text in any::<String>()) {
        for token in tokenize(&text) {
            prop_assert!(utf16_len(&token) >= 2 && utf16_len(&token) <= 40);
            prop_assert_eq!(token.to_lowercase(), token);
        }
    }

    #[test]
    fn queries_and_tags_never_panic(text in any::<String>()) {
        let _ = parse_query(&text);
        let _ = extract_tags(&text);
    }

    #[test]
    fn every_indexed_word_finds_its_note(words in proptest::collection::vec("[a-z]{2,12}", 1..20)) {
        let mut index = SearchIndex::new();
        let content = words.join(" ");
        index.add(&IndexDocument { id: "n".into(), title: String::new(), content, updated_at: 0, folder_id: None });
        for word in &words {
            prop_assert_eq!(index.search(word, 10).len(), 1);
        }
        index.remove("n");
        prop_assert_eq!(index.token_count(), 0);
    }
}
