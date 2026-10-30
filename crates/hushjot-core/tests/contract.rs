//! The format contract: every case in the HushJot app's frozen
//! `contract/format-vectors.json` must hold here.
//!
//! These bytes were sealed by the app's TypeScript core. A failure means this
//! crate would read or write notes the app disagrees with — fix the port, never
//! the vectors. `tools/sync-contract.sh` re-vendors the file from the app repo,
//! and CI fails if the copy here drifts from it.

use std::path::PathBuf;

use hushjot_core::bytes::from_base64;
use hushjot_core::envelope::{open, open_text, record_context, Key, RecordKind};
use hushjot_core::key_record::KeyRecord;
use hushjot_core::meta::{note_meta_json, read_note_meta, FolderPayload};
use hushjot_core::notes::{derive_title, excerpt_of};
use hushjot_core::search::tokenize;
use serde_json::Value;

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contract/format-vectors.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("contract/format-vectors.json")).unwrap()
}

fn record(v: &Value, id: &str) -> (KeyRecord, String, String) {
    let entry = v["keyRecords"].as_array().unwrap().iter().find(|r| r["id"] == id).unwrap();
    (
        KeyRecord::from_json(&entry["record"]).unwrap(),
        entry["passphrase"].as_str().unwrap().to_string(),
        entry["recoveryCode"].as_str().unwrap().to_string(),
    )
}

/// The DEK every sealed vector was sealed under.
fn sealing_key(v: &Value) -> Key {
    let (rec, passphrase, _) = record(v, v["sealedUnder"].as_str().unwrap());
    rec.unlock_with_passphrase(&passphrase).unwrap()
}

#[test]
fn the_format_version_matches() {
    assert_eq!(vectors()["formatVersion"], hushjot_core::FORMAT_VERSION);
}

#[test]
fn the_fast_record_unlocks_with_passphrase_and_recovery_code_to_one_key() {
    let v = vectors();
    let (rec, passphrase, code) = record(&v, "fast");
    let by_passphrase = rec.unlock_with_passphrase(&passphrase).unwrap();
    let by_code = rec.unlock_with_recovery_code(&code).unwrap();
    assert_eq!(by_passphrase, by_code);
    assert!(rec.unlock_with_passphrase(&format!("{passphrase} ")).is_err());
    // Lowercase and spaced: normalisation must still reach the key.
    assert_eq!(rec.unlock_with_recovery_code(&code.to_lowercase().replace('-', " ")).unwrap(), by_code);
}

#[test]
fn the_default_record_unlocks_at_production_parameters() {
    let v = vectors();
    let (rec, passphrase, code) = record(&v, "default");
    assert_eq!(rec.passphrase.memory_kib, 65_536);
    assert_eq!(rec.unlock_with_passphrase(&passphrase).unwrap(), rec.unlock_with_recovery_code(&code).unwrap());
}

#[test]
fn key_records_round_trip_through_serialisation() {
    let v = vectors();
    for entry in v["keyRecords"].as_array().unwrap() {
        let rec = KeyRecord::from_json(&entry["record"]).unwrap();
        assert_eq!(serde_json::to_value(rec.to_serialized()).unwrap(), entry["record"]);
    }
}

#[test]
fn every_note_meta_vector_opens_and_reserialises_byte_for_byte() {
    let v = vectors();
    let key = sealing_key(&v);
    for case in v["noteMeta"].as_array().unwrap() {
        let plain = open(&key, &from_base64(case["envelopeBase64"].as_str().unwrap()).unwrap(), None).unwrap();
        let json = case["json"].as_str().unwrap();
        assert_eq!(std::str::from_utf8(&plain).unwrap(), json, "{}", case["id"]);
        let payload = read_note_meta(&case["payload"]);
        assert_eq!(note_meta_json(&payload), json, "{}", case["id"]);
        let reread: Value = serde_json::from_str(json).unwrap();
        assert_eq!(note_meta_json(&read_note_meta(&reread)), json, "{}", case["id"]);
    }
}

#[test]
fn every_body_vector_opens() {
    let v = vectors();
    let key = sealing_key(&v);
    for case in v["body"].as_array().unwrap() {
        let sealed = from_base64(case["envelopeBase64"].as_str().unwrap()).unwrap();
        assert_eq!(open_text(&key, &sealed, None).unwrap(), case["text"].as_str().unwrap(), "{}", case["id"]);
    }
}

#[test]
fn every_folder_vector_opens_and_reserialises() {
    let v = vectors();
    let key = sealing_key(&v);
    for case in v["folder"].as_array().unwrap() {
        let plain = open(&key, &from_base64(case["envelopeBase64"].as_str().unwrap()).unwrap(), None).unwrap();
        assert_eq!(std::str::from_utf8(&plain).unwrap(), case["json"].as_str().unwrap());
        let payload: FolderPayload = serde_json::from_value(case["payload"].clone()).unwrap();
        assert_eq!(payload.to_json(), case["json"].as_str().unwrap());
    }
}

#[test]
fn every_bound_vector_opens_only_under_its_own_context() {
    let v = vectors();
    let key = sealing_key(&v);
    let kinds = [
        ("note-meta", RecordKind::NoteMeta),
        ("note-body", RecordKind::NoteBody),
        ("folder", RecordKind::Folder),
        ("share-key", RecordKind::ShareKey),
    ];
    for case in v["bound"].as_array().unwrap() {
        let kind = kinds.iter().find(|(name, _)| case["kind"] == *name).unwrap().1;
        let context = record_context(kind, case["recordId"].as_str().unwrap());
        assert_eq!(context, case["context"].as_str().unwrap());
        let sealed = from_base64(case["envelopeBase64"].as_str().unwrap()).unwrap();
        assert_eq!(&sealed[..2], &[2, 1]);
        let plain = open(&key, &sealed, Some(&context)).unwrap();
        assert_eq!(std::str::from_utf8(&plain).unwrap(), case["plaintext"].as_str().unwrap(), "{}", case["id"]);
        assert!(open(&key, &sealed, None).is_err());
        assert!(open(&key, &sealed, Some(&format!("{context}x"))).is_err());
    }
}

#[test]
fn derived_titles_excerpts_and_tokens_agree() {
    let v = vectors();
    for case in v["deriveTitle"].as_array().unwrap() {
        assert_eq!(derive_title(case["content"].as_str().unwrap()), case["expected"].as_str().unwrap());
    }
    for case in v["excerptOf"].as_array().unwrap() {
        assert_eq!(excerpt_of(case["content"].as_str().unwrap()), case["expected"].as_str().unwrap());
    }
    for case in v["tokenize"].as_array().unwrap() {
        let expected: Vec<String> = serde_json::from_value(case["expected"].clone()).unwrap();
        assert_eq!(tokenize(case["text"].as_str().unwrap()), expected);
    }
}

#[test]
fn the_envelope_constants_match() {
    let v = vectors();
    assert_eq!(v["envelope"]["headerBytes"], 14);
    assert_eq!(v["envelope"]["tagBytes"], 16);
    assert_eq!(v["envelope"]["aadHex"], "0101");
    assert_eq!(v["boundEnvelope"]["aadPrefixHex"], "0201");
}
