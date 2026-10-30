//! The emulator journey: the real REST client against the app's real security
//! rules, in the Firebase emulator.
//!
//! The in-memory fake proves the engine's decisions; this proves the **wire**.
//! Several constraints — `syncedAt` as a server transform, full-replace writes,
//! `bytesValue` ciphertext, the meter's exact deltas, the version and share
//! commit shapes — are invisible until a real rules engine refuses a real
//! request. It unlocks the app's frozen `fast` key record, so it proves this
//! crate opens a vault the *app* created.
//!
//! Skipped unless `HUSHJOT_EMULATOR=1`. Run it with `tools/emulator-test.sh`,
//! which starts the emulators with `contract/firestore.rules`.

use std::sync::Arc;

use serde_json::{json, Value};

use hushjot::store::MemoryStore;
use hushjot::{Config, HushJot, ShareExpiry, SyncEvent, SyncStatus, UnlockOptions, Vault};

const PROJECT: &str = "teksto-dev";
const AUTH: &str = "127.0.0.1:9097";
const FIRESTORE: &str = "127.0.0.1:8087";
const PASSWORD: &str = "journey-password-1";

fn enabled() -> bool {
    std::env::var("HUSHJOT_EMULATOR").is_ok_and(|v| v == "1")
}

fn docs_url() -> String {
    format!("http://{FIRESTORE}/v1/projects/{PROJECT}/databases/(default)/documents")
}

fn vectors() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/format-vectors.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn fast() -> (Value, String) {
    let v = vectors();
    let fast = v["keyRecords"].as_array().unwrap().iter().find(|r| r["id"] == "fast").unwrap().clone();
    (fast["record"].clone(), fast["passphrase"].as_str().unwrap().to_string())
}

fn to_value(v: &Value) -> Value {
    match v {
        Value::Null => json!({ "nullValue": null }),
        Value::Bool(b) => json!({ "booleanValue": b }),
        Value::Number(n) if n.is_i64() => json!({ "integerValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n.as_f64() }),
        Value::String(s) => json!({ "stringValue": s }),
        Value::Array(a) => json!({ "arrayValue": { "values": a.iter().map(to_value).collect::<Vec<_>>() } }),
        Value::Object(o) => {
            let fields: serde_json::Map<String, Value> = o.iter().map(|(k, v)| (k.clone(), to_value(v))).collect();
            json!({ "mapValue": { "fields": fields } })
        }
    }
}

struct Admin {
    http: reqwest::Client,
}

impl Admin {
    async fn patch(&self, path: &str, fields: Value, mask: &[&str]) {
        let query: String = mask.iter().map(|m| format!("updateMask.fieldPaths={m}&")).collect();
        let r = self
            .http
            .patch(format!("{}/{path}?{query}", docs_url()))
            .bearer_auth("owner")
            .json(&json!({ "fields": fields }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "admin PATCH {path}: {}", r.text().await.unwrap());
    }

    async fn get(&self, path: &str) -> Option<Value> {
        let r = self.http.get(format!("{}/{path}", docs_url())).bearer_auth("owner").send().await.unwrap();
        if r.status().as_u16() == 404 {
            return None;
        }
        Some(r.json().await.unwrap())
    }

    async fn list(&self, path: &str) -> Vec<Value> {
        let r: Value = self
            .http
            .get(format!("{}/{path}?pageSize=1000", docs_url()))
            .bearer_auth("owner")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["documents"].as_array().cloned().unwrap_or_default()
    }

    async fn set_rules(&self, content: &str) {
        let r = self
            .http
            .put(format!("http://{FIRESTORE}/emulator/v1/projects/{PROJECT}:securityRules"))
            .bearer_auth("owner")
            .json(&json!({ "rules": { "files": [{ "name": "firestore.rules", "content": content }] } }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "rules swap: {}", r.text().await.unwrap());
    }
}

async fn sign_up(http: &reqwest::Client, email: &str) -> String {
    let r: Value = http
        .post(format!("http://{AUTH}/identitytoolkit.googleapis.com/v1/accounts:signUp?key=emulator"))
        .json(&json!({ "email": email, "password": PASSWORD, "returnSecureToken": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r["localId"].as_str().expect("signUp").to_string()
}

async fn device(email: &str, passphrase: &str) -> Vault {
    let hj = HushJot::builder()
        .config(Config::emulator(PROJECT, AUTH, FIRESTORE))
        .store(MemoryStore::new())
        .build()
        .unwrap();
    let account = hj.sign_in(email, PASSWORD).await.unwrap();
    account.unlock(passphrase, UnlockOptions::default()).await.unwrap()
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<SyncEvent>) -> Vec<SyncEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn journey_against_the_real_rules() {
    if !enabled() {
        eprintln!("skipped: set HUSHJOT_EMULATOR=1 (see tools/emulator-test.sh)");
        return;
    }
    let http = reqwest::Client::new();
    let admin = Admin { http: http.clone() };
    let email = format!("rs-journey-{}@example.com", uuid::Uuid::new_v4());
    let uid = sign_up(&http, &email).await;
    admin.patch(&format!("allowlist/{email}"), json!({ "active": { "booleanValue": true } }), &[]).await;
    let (record, passphrase) = fast();
    admin
        .patch(
            &format!("users/{uid}/keys/primary"),
            json!({ "v": { "integerValue": "1" }, "record": to_value(&record), "updatedAt": { "integerValue": "1" } }),
            &[],
        )
        .await;

    // 1. Sign in, unlock the app's frozen record, create, push.
    let a = device(&email, &passphrase).await;
    let folder = a.folders().create("PlainFolderMarker", None).unwrap();
    let id = a.notes().create("# PlainTitleMarker\nPlainBodyMarker", Some(&folder)).unwrap();
    assert_eq!(a.sync_now().await, SyncStatus::Idle, "the rules accepted the first push");
    let stored = admin.get(&format!("users/{uid}/notes/{id}")).await.expect("note stored");
    assert_eq!(stored["fields"]["revision"]["integerValue"], "1");
    assert!(stored["fields"]["meta"]["bytesValue"].is_string());
    let usage = admin.get(&format!("users/{uid}/usage/current")).await.expect("meter");
    assert_eq!(usage["fields"]["pn"]["integerValue"], "2", "one push per write");

    // 2. Edits advance the server revision by exactly one per push.
    a.notes().update(&id, "# PlainTitleMarker\nPlainBodyMarker v2").unwrap();
    a.notes().update(&id, "# PlainTitleMarker\nPlainBodyMarker v3").unwrap();
    assert_eq!(a.sync_now().await, SyncStatus::Idle);
    let stored = admin.get(&format!("users/{uid}/notes/{id}")).await.unwrap();
    assert_eq!(stored["fields"]["revision"]["integerValue"], "2");
    assert_eq!(
        admin.list(&format!("users/{uid}/notes/{id}/versions")).await.len(),
        1,
        "the session's first text went up as a version"
    );

    // 3. A second device pulls it and can read it.
    let b = device(&email, &passphrase).await;
    assert_eq!(b.sync_now().await, SyncStatus::Idle);
    assert_eq!(b.notes().get(&id).unwrap().content, "# PlainTitleMarker\nPlainBodyMarker v3");
    assert_eq!(b.folders().path(&folder).unwrap(), "PlainFolderMarker");

    // 4. A real compare-and-swap race becomes a conflict copy, accepted by the rules.
    a.notes().update(&id, "# PlainTitleMarker\nfrom A").unwrap();
    b.notes().update(&id, "# PlainTitleMarker\nfrom B").unwrap();
    assert_eq!(a.sync_now().await, SyncStatus::Idle);
    let mut events = b.subscribe();
    assert_eq!(b.sync_now().await, SyncStatus::Idle);
    assert!(drain(&mut events).iter().any(|e| matches!(e, SyncEvent::ConflictCopy { .. })));
    assert_eq!(b.notes().list().unwrap().len(), 2);

    // 5. A pin racing an edit rebases without a copy.
    a.sync_now().await;
    a.notes().update(&id, "# PlainTitleMarker\nedited on A").unwrap();
    b.notes().set_pinned(&id, true).unwrap();
    a.sync_now().await;
    assert_eq!(b.sync_now().await, SyncStatus::Idle);
    assert_eq!(b.notes().list().unwrap().len(), 2, "still the original and the one earlier copy");
    assert!(b.notes().get(&id).unwrap().summary.pinned_at.is_some());

    // 6. Delete: the version, then the tombstone, both accepted; undelete elsewhere.
    b.notes().delete(&id).unwrap();
    assert_eq!(b.sync_now().await, SyncStatus::Idle);
    let stored = admin.get(&format!("users/{uid}/notes/{id}")).await.unwrap();
    assert_eq!(stored["fields"]["deleted"]["booleanValue"], true);
    assert!(stored["fields"].get("body").is_none());
    a.sync_now().await;
    assert!(a.notes().get(&id).is_err());
    a.notes().undelete(&id).await.unwrap();
    assert_eq!(a.sync_now().await, SyncStatus::Idle);
    assert!(!a.history(&id).list().await.unwrap().is_empty());

    // 7. Share: one commit for both documents and the meter; the public blob is
    //    readable with no credentials; revoke removes it.
    let share = a.shares().publish(&id, ShareExpiry::Hour).await.unwrap();
    let r = hushjot::core::share::parse_fragment(&share.url).unwrap();
    let public: Value = http.get(format!("{}/shares/{}", docs_url(), r.id)).send().await.unwrap().json().await.unwrap();
    let payload =
        hushjot::core::bytes::from_base64(public["fields"]["payload"]["bytesValue"].as_str().unwrap()).unwrap();
    let opened =
        hushjot::core::share::open_share(&hushjot::core::share::import_share_key(&r.key).unwrap(), &payload).unwrap();
    assert!(opened.body.contains("edited on A"));
    assert_eq!(a.shares().list().await.unwrap().len(), 1);
    a.shares().revoke(&share.id).await.unwrap();
    assert!(admin.get(&format!("shares/{}", r.id)).await.is_none());

    // 8. Nothing readable ever reached the server.
    let mut dump = String::new();
    for col in ["notes", "folders", "shares"] {
        dump.push_str(&serde_json::to_string(&admin.list(&format!("users/{uid}/{col}")).await).unwrap());
    }
    for note in admin.list(&format!("users/{uid}/notes")).await {
        let name = note["name"].as_str().unwrap();
        let note_id = name.rsplit('/').next().unwrap();
        dump.push_str(
            &serde_json::to_string(&admin.list(&format!("users/{uid}/notes/{note_id}/versions")).await).unwrap(),
        );
    }
    for marker in ["PlainFolderMarker", "PlainTitleMarker", "PlainBodyMarker"] {
        assert!(!dump.contains(marker), "{marker} reached the server in the clear");
    }

    // 9. A full push window holds the change; a day later the reset is accepted.
    let now = chrono::Utc::now();
    admin
        .patch(
            &format!("users/{uid}/usage/current"),
            json!({ "pn": { "integerValue": "20000" }, "ws": { "timestampValue": now.to_rfc3339() } }),
            &["pn", "ws"],
        )
        .await;
    let held = a.notes().create("held by the push limit", None).unwrap();
    let mut events = a.subscribe();
    a.sync_now().await;
    assert!(admin.get(&format!("users/{uid}/notes/{held}")).await.is_none());
    assert!(drain(&mut events).iter().any(|e| matches!(e, SyncEvent::PushLimit { waiting: 1, .. })));
    let yesterday = now - chrono::Duration::hours(25);
    admin
        .patch(
            &format!("users/{uid}/usage/current"),
            json!({ "ws": { "timestampValue": yesterday.to_rfc3339() } }),
            &["ws"],
        )
        .await;
    let c = device(&email, &passphrase).await;
    let late = c.notes().create("after the window", None).unwrap();
    assert_eq!(c.sync_now().await, SyncStatus::Idle);
    assert!(admin.get(&format!("users/{uid}/notes/{late}")).await.is_some(), "the reset was accepted");

    // 10. A full account holds growth and says so.
    admin
        .patch(
            &format!("users/{uid}/usage/current"),
            json!({ "bytes": { "integerValue": (1i64 << 30).to_string() } }),
            &["bytes"],
        )
        .await;
    let big = c.notes().create("does not fit", None).unwrap();
    let mut events = c.subscribe();
    c.sync_now().await;
    assert!(admin.get(&format!("users/{uid}/notes/{big}")).await.is_none());
    assert!(drain(&mut events).iter().any(|e| matches!(e, SyncEvent::StorageFull { .. })));
    admin.patch(&format!("users/{uid}/usage/current"), json!({ "bytes": { "integerValue": "0" } }), &["bytes"]).await;

    // 11. A lockdown is reported as a pause, never as a lapsed plan.
    let live_rules =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/firestore.rules")).unwrap();
    admin
        .set_rules(
            "rules_version = '2';\nservice cloud.firestore {\n  match /databases/{database}/documents {\n    match /status/lockdown { allow get: if true; }\n    match /{document=**} { allow read, write: if false; }\n  }\n}\n",
        )
        .await;
    c.notes().create("during the pause", None).unwrap();
    assert_eq!(c.sync_now().await, SyncStatus::Paused);
    admin.set_rules(&live_rules).await;

    // 12. Cross-client: what this SDK writes, @hushjot/cli reads, and back.
    cross_client(&email, &passphrase, &c).await;

    // 13. Revoking the invitation is NotApproved, and local work stays queued.
    admin.patch(&format!("allowlist/{email}"), json!({ "active": { "booleanValue": false } }), &["active"]).await;
    let d = device_unchecked(&email).await;
    assert!(matches!(d, Err(hushjot::Error::NotApproved)));
}

/// The CLI checkout, if it sits beside this repository (or `HUSHJOT_CLI_DIR`).
fn cli_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var("HUSHJOT_CLI_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../hushjot-cli")));
    dir.join("src/bin/hushjot.ts").exists().then_some(dir)
}

fn cli(dir: &std::path::Path, home: &std::path::Path, passphrase: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("node")
        .arg(dir.join("src/bin/hushjot.ts"))
        .args(args)
        .env("HUSHJOT_HOME", home)
        .env("HUSHJOT_AUTH_HOST", AUTH)
        .env("HUSHJOT_FIRESTORE_HOST", FIRESTORE)
        .env("HUSHJOT_PASSWORD", PASSWORD)
        .env("HUSHJOT_PASSPHRASE", passphrase)
        .env("HUSHJOT_SECRET_STORE", "environment")
        .output()
        .expect("node runs");
    assert!(out.status.success(), "hushjot {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

async fn cross_client(email: &str, passphrase: &str, vault: &Vault) {
    let Some(dir) = cli_dir() else {
        eprintln!("cross-client step skipped: no hushjot-cli checkout beside this repo");
        return;
    };
    let home = std::env::temp_dir().join(format!("hushjot-rs-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    cli(&dir, &home, passphrase, &["login", "--project", PROJECT, "--api-key", "emulator", "--email", email]);

    // The SDK writes; the CLI reads the same title and text.
    let id = vault.notes().create("# Written in Rust\nread by the CLI", None).unwrap();
    assert_eq!(vault.sync_now().await, SyncStatus::Idle);
    let read = cli(&dir, &home, passphrase, &["cat", &id]);
    assert!(read.contains("read by the CLI"), "CLI read: {read}");

    // The CLI writes (a push and an append); the SDK reads both.
    let pushed: Value =
        serde_json::from_str(&cli(&dir, &home, passphrase, &["push", "--json", "# From the CLI\nfirst line"])).unwrap();
    let cli_id = pushed["id"].as_str().unwrap().to_string();
    cli(&dir, &home, passphrase, &["append", &id, "appended by the CLI"]);
    assert_eq!(vault.sync_now().await, SyncStatus::Idle);
    let note = vault.notes().get(&cli_id).unwrap();
    assert_eq!(note.summary.title, "From the CLI");
    assert!(note.summary.created_at > 0);
    assert!(vault.notes().get(&id).unwrap().content.ends_with("appended by the CLI"));

    // And the SDK can keep editing a note the CLI changed: the revisions agree.
    vault.notes().update(&cli_id, "# From the CLI\nedited in Rust").unwrap();
    assert_eq!(vault.sync_now().await, SyncStatus::Idle);
    assert!(cli(&dir, &home, passphrase, &["cat", &cli_id]).contains("edited in Rust"));
    let _ = std::fs::remove_dir_all(home);
}

async fn device_unchecked(email: &str) -> hushjot::Result<Vault> {
    let hj = HushJot::builder().config(Config::emulator(PROJECT, AUTH, FIRESTORE)).build()?;
    let account = hj.sign_in(email, PASSWORD).await?;
    account.unlock("unused", UnlockOptions::default()).await
}

#[allow(dead_code)]
fn keep_arc_import(_: Arc<()>) {}
