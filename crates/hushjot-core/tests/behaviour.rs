//! The behaviour contract: every case in the app's
//! `contract/behaviour-vectors.json` must hold here.
//!
//! Where `contract.rs` checks stored bytes, this checks derivations the app
//! computes and this crate must compute identically — the query grammar, tags,
//! folder resolution, list order, search ranking (ties included), titles,
//! excerpts and the meter. The file is generated from the app's live core, so
//! a failure here means the port has drifted from the app. Fix the port.

use std::collections::HashSet;
use std::path::PathBuf;

use hushjot_core::folders::{self, Folder};
use hushjot_core::history::seal_id_of;
use hushjot_core::notes::{self, Listable, NoteSort, SortKey};
use hushjot_core::search::{self, IndexDocument, SearchIndex, SearchOptions};
use hushjot_core::tags;
use hushjot_core::usage;
use serde_json::{json, Value};

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contract/behaviour-vectors.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("contract/behaviour-vectors.json")).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    serde_json::from_value(v.clone()).unwrap()
}

#[test]
fn titles_excerpts_and_tokens() {
    let v = vectors();
    for case in v["deriveTitle"].as_array().unwrap() {
        assert_eq!(
            notes::derive_title(s(&case["content"])),
            s(&case["expected"]),
            "deriveTitle({:?})",
            case["content"]
        );
    }
    for case in v["excerptOf"].as_array().unwrap() {
        assert_eq!(notes::excerpt_of(s(&case["content"])), s(&case["expected"]), "excerptOf({:?})", case["content"]);
    }
    for case in v["tokenize"].as_array().unwrap() {
        assert_eq!(search::tokenize(s(&case["text"])), strings(&case["expected"]), "tokenize({:?})", case["text"]);
    }
}

#[test]
fn the_query_grammar() {
    let v = vectors();
    for case in v["parseQuery"].as_array().unwrap() {
        let parsed = search::parse_query(s(&case["query"]));
        let as_json = json!({
            "terms": parsed.terms, "notTerms": parsed.not_terms,
            "phrases": parsed.phrases, "notPhrases": parsed.not_phrases,
            "tags": parsed.tags, "notTags": parsed.not_tags,
            "folders": parsed.folders, "notFolders": parsed.not_folders,
        });
        assert_eq!(as_json, case["expected"], "parseQuery({:?})", case["query"]);
        assert_eq!(search::has_positive_term(&parsed), case["hasPositiveTerm"].as_bool().unwrap());
    }
    for case in v["normalisePhrase"].as_array().unwrap() {
        assert_eq!(search::normalise_phrase(s(&case["text"])), s(&case["expected"]));
    }
}

#[test]
fn tags() {
    let v = vectors();
    for case in v["extractTags"].as_array().unwrap() {
        let found: Vec<Value> = tags::extract_tags(s(&case["content"]))
            .into_iter()
            .map(|t| json!({ "key": t.key, "label": t.label }))
            .collect();
        assert_eq!(Value::Array(found), case["expected"], "extractTags({:?})", case["content"]);
    }
    for case in v["normaliseTag"].as_array().unwrap() {
        assert_eq!(json!(tags::normalise_tag(s(&case["body"]))), case["expected"], "normaliseTag({:?})", case["body"]);
    }
    for case in v["tagMatches"].as_array().unwrap() {
        assert_eq!(tags::tag_matches(s(&case["key"]), s(&case["filter"])), case["expected"].as_bool().unwrap());
    }
}

fn fixture_folders(v: &Value) -> Vec<Folder> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| Folder {
            id: s(&f["id"]).into(),
            name: s(&f["name"]).into(),
            parent_id: f["parentId"].as_str().map(Into::into),
            created_at: f["createdAt"].as_i64().unwrap(),
            updated_at: f["updatedAt"].as_i64().unwrap(),
        })
        .collect()
}

#[test]
fn folder_resolution_and_the_tree() {
    let v = vectors();
    let f = &v["folders"];
    let fixture = fixture_folders(&f["fixture"]);
    for case in f["foldersMatching"].as_array().unwrap() {
        let got = folders::folders_matching(&fixture, s(&case["wanted"]), case["prefix"].as_bool().unwrap());
        assert_eq!(got, strings(&case["expected"]), "foldersMatching({:?}, prefix {})", case["wanted"], case["prefix"]);
    }
    for case in f["subtreeIds"].as_array().unwrap() {
        assert_eq!(
            folders::subtree_ids(&fixture, s(&case["id"])),
            strings(&case["expected"]),
            "subtree {}",
            case["id"]
        );
    }
    for case in f["ancestorIds"].as_array().unwrap() {
        assert_eq!(
            folders::ancestor_ids(&fixture, s(&case["id"])),
            strings(&case["expected"]),
            "ancestors {}",
            case["id"]
        );
    }
    for case in f["folderPath"].as_array().unwrap() {
        assert_eq!(folders::folder_path(&fixture, s(&case["id"]), " / "), s(&case["expected"]));
    }
    for case in f["canMoveFolder"].as_array().unwrap() {
        let got = folders::can_move_folder(&fixture, s(&case["id"]), case["parent"].as_str());
        assert_eq!(got, case["expected"].as_bool().unwrap(), "canMoveFolder {} → {:?}", case["id"], case["parent"]);
    }
}

#[derive(Clone)]
struct Row(String, String, Option<i64>, i64, i64);

impl Listable for Row {
    fn id(&self) -> &str {
        &self.0
    }
    fn title(&self) -> &str {
        &self.1
    }
    fn pinned_at(&self) -> Option<i64> {
        self.2
    }
    fn updated_at(&self) -> i64 {
        self.3
    }
    fn created_at(&self) -> i64 {
        self.4
    }
}

#[test]
fn list_order_under_every_sort() {
    let v = vectors();
    let rows: Vec<Row> = v["orderForList"]["fixture"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            Row(
                s(&n["id"]).into(),
                s(&n["title"]).into(),
                n["pinnedAt"].as_i64(),
                n["updatedAt"].as_i64().unwrap(),
                n["createdAt"].as_i64().unwrap(),
            )
        })
        .collect();
    for case in v["orderForList"]["cases"].as_array().unwrap() {
        let key = match s(&case["sort"]["key"]) {
            "created" => SortKey::Created,
            "title" => SortKey::Title,
            _ => SortKey::Edited,
        };
        let sort = NoteSort { key, reversed: case["sort"]["reversed"].as_bool().unwrap() };
        let ids: Vec<String> = notes::order_for_list(&rows, sort).into_iter().map(|r| r.0).collect();
        assert_eq!(ids, strings(&case["expected"]), "orderForList {:?}", case["sort"]);
    }
}

fn folder_set(v: &Value) -> Option<HashSet<Option<String>>> {
    v.as_array().map(|ids| ids.iter().map(|id| id.as_str().map(Into::into)).collect())
}

#[test]
fn search_ranking_over_a_fixed_corpus() {
    let v = vectors();
    let mut index = SearchIndex::new();
    for doc in v["search"]["corpus"].as_array().unwrap() {
        index.add(&IndexDocument {
            id: s(&doc["id"]).into(),
            title: s(&doc["title"]).into(),
            content: s(&doc["content"]).into(),
            updated_at: doc["updatedAt"].as_i64().unwrap(),
            folder_id: doc["folderId"].as_str().map(Into::into),
        });
    }
    for case in v["search"]["cases"].as_array().unwrap() {
        let options = SearchOptions {
            limit: Some(case["limit"].as_u64().unwrap() as usize),
            folder_ids: folder_set(&case["folderIds"]),
            not_folder_ids: folder_set(&case["notFolderIds"]),
        };
        let hits = index.search_parsed(&search::parse_query(s(&case["query"])), &options);
        let expected = case["expected"].as_array().unwrap();
        let got: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        let want: Vec<&str> = expected.iter().map(|h| s(&h["id"])).collect();
        assert_eq!(got, want, "search {:?}", case["query"]);
        for (hit, exp) in hits.iter().zip(expected) {
            assert!(
                (hit.score - exp["score"].as_f64().unwrap()).abs() < 1e-12,
                "score for {} in {:?}",
                hit.id,
                case["query"]
            );
            assert_eq!(hit.in_title, exp["inTitle"].as_bool().unwrap());
        }
    }
    let summary: Vec<Value> =
        index.tag_summary().into_iter().map(|t| json!({ "key": t.key, "label": t.label, "count": t.count })).collect();
    assert_eq!(Value::Array(summary), v["search"]["tagSummary"]);
}

#[test]
fn meter_arithmetic_and_seal_ids() {
    let v = vectors();
    let m = &v["meter"];
    for case in m["meterDelta"].as_array().unwrap() {
        assert_eq!(
            usage::meter_delta(case["next"].as_i64().unwrap(), case["old"].as_i64().unwrap()),
            case["expected"].as_i64().unwrap()
        );
    }
    for case in m["chargedBytes"].as_array().unwrap() {
        assert_eq!(usage::charged_bytes(case["size"].as_i64().unwrap()), case["expected"].as_i64().unwrap());
    }
    for case in m["usageMonth"].as_array().unwrap() {
        assert_eq!(usage::usage_month(case["ms"].as_i64().unwrap()), s(&case["expected"]));
    }
    for case in v["sealIdOf"].as_array().unwrap() {
        let bytes: Vec<u8> = serde_json::from_value(case["bytes"].clone()).unwrap();
        assert_eq!(json!(seal_id_of(&bytes)), case["expected"]);
    }
}
