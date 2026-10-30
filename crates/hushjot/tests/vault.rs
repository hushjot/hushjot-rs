//! Unlocking, the store binding, history and shares.

mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use hushjot::backend::KeyRecordDoc;
use hushjot::core::history::VersionReason;
use hushjot::core::share::{import_share_key, open_share, parse_fragment};
use hushjot::{Config, Error, HushJot, MemorySecrets, MemoryStore, SecretStore, ShareExpiry, UnlockOptions};
use support::{device, device_with_store, FakeServer, PASSPHRASE, RECOVERY};

async fn account(
    server: &Arc<FakeServer>,
    store: Arc<dyn hushjot::Store>,
    secrets: Arc<MemorySecrets>,
) -> hushjot::Account {
    let hj = HushJot::builder().store_arc(store).build().unwrap();
    let _ = secrets;
    hj.account_with_backend("Me@Example.com", server.clone()).await
}

#[tokio::test]
async fn the_wrong_passphrase_is_refused_and_the_recovery_code_works() {
    let server = FakeServer::new();
    let acct = account(&server, Arc::new(MemoryStore::new()), Arc::new(MemorySecrets::new())).await;
    assert_eq!(acct.email(), "me@example.com");
    assert!(matches!(acct.unlock("wrong", UnlockOptions::default()).await, Err(Error::WrongSecret)));
    let vault = acct.unlock_with_recovery_code(&RECOVERY.to_lowercase(), UnlockOptions::default()).await.unwrap();
    assert_eq!(vault.uid(), "uid-1");
}

#[tokio::test]
async fn an_account_without_a_vault_or_with_a_reset_one_says_so() {
    let server = FakeServer::new();
    server.state().key_record = Some(KeyRecordDoc::Missing);
    let acct = account(&server, Arc::new(MemoryStore::new()), Arc::new(MemorySecrets::new())).await;
    assert!(matches!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await, Err(Error::NoVault)));
    server.state().key_record = Some(KeyRecordDoc::Tombstone);
    assert!(matches!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await, Err(Error::VaultReset)));
}

#[tokio::test]
async fn a_store_bound_to_another_vault_is_refused() {
    let server = FakeServer::new();
    let store: Arc<dyn hushjot::Store> = Arc::new(MemoryStore::new());
    let first = device_with_store(&server, store.clone()).await;
    first.vault.notes().create("old vault note", None).unwrap();

    // The account is reset elsewhere and gets a new vault.
    let other = FakeServer::new();
    server.state().key_record =
        Some(KeyRecordDoc::Record(Box::new(hushjot::core::key_record::KeyRecord { created_at: 42, ..other.record() })));
    let acct = account(&server, store.clone(), Arc::new(MemorySecrets::new())).await;
    assert!(matches!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await, Err(Error::VaultReset)));

    // Wiping the local store lets the device start again.
    first.hj.wipe_local_data().unwrap();
    assert!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await.is_ok());
}

#[tokio::test]
async fn a_store_bound_to_another_account_is_refused() {
    let server = FakeServer::new();
    let store: Arc<dyn hushjot::Store> = Arc::new(MemoryStore::new());
    store.set("app:owner", Some("someone-else")).unwrap();
    let acct = account(&server, store, Arc::new(MemorySecrets::new())).await;
    assert!(matches!(
        acct.unlock(PASSPHRASE, UnlockOptions::default()).await,
        Err(Error::StoreBelongsToAnotherAccount)
    ));
}

#[tokio::test]
async fn staying_unlocked_caches_the_key_until_the_passphrase_changes() {
    let server = FakeServer::new();
    let secrets = MemorySecrets::new();
    let hj = HushJot::builder().secrets(secrets).build().unwrap();
    let acct = hj.account_with_backend("me@example.com", server.clone()).await;
    assert!(acct.unlock_cached().await.unwrap().is_none());
    acct.unlock(PASSPHRASE, UnlockOptions { stay_unlocked: true }).await.unwrap();
    assert!(acct.unlock_cached().await.unwrap().is_some());

    // A passphrase change elsewhere rewrites the wrap, so the cache misses.
    let dek = server.dek.clone();
    let params = hushjot::core::kdf::Argon2Params { memory_kib: 64, iterations: 1, ..Default::default() };
    let changed = hushjot::core::key_record::create_key_record_for_tests(
        &dek,
        "new pass",
        RECOVERY,
        &params,
        server.record().created_at,
    );
    server.state().key_record = Some(KeyRecordDoc::Record(Box::new(changed)));
    assert!(acct.unlock_cached().await.unwrap().is_none());
    assert!(acct.unlock("new pass", UnlockOptions::default()).await.is_ok());
}

#[tokio::test]
async fn offline_unlock_uses_the_last_key_record_this_store_fetched() {
    let server = FakeServer::new();
    let store: Arc<dyn hushjot::Store> = Arc::new(MemoryStore::new());
    let hj = HushJot::builder().store_arc(store.clone()).secrets(MemorySecrets::new()).build().unwrap();
    let acct = hj.account_with_backend("me@example.com", server.clone()).await;
    acct.unlock(PASSPHRASE, UnlockOptions { stay_unlocked: true }).await.unwrap();

    server.offline.store(true, Ordering::SeqCst);
    assert!(acct.unlock_cached().await.unwrap().is_some());
    assert!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await.is_ok());
    assert!(matches!(acct.unlock("wrong", UnlockOptions::default()).await, Err(Error::WrongSecret)));

    // A copy kept for another account is never used.
    store.set("app:owner", Some("someone-else")).unwrap();
    assert!(matches!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await, Err(Error::Transient(_))));
}

#[tokio::test]
async fn offline_unlock_without_a_cached_record_reports_the_network() {
    let server = FakeServer::new();
    server.offline.store(true, Ordering::SeqCst);
    let acct = account(&server, Arc::new(MemoryStore::new()), Arc::new(MemorySecrets::new())).await;
    assert!(matches!(acct.unlock(PASSPHRASE, UnlockOptions::default()).await, Err(Error::Transient(_))));
}

#[tokio::test]
async fn resume_works_offline_once_the_uid_is_saved() {
    let unreachable = Config::emulator("hushjot-test", "127.0.0.1:1", "127.0.0.1:1");

    let secrets = MemorySecrets::new();
    secrets.set("me@example.com", "refresh", "saved-refresh-token").unwrap();
    let hj = HushJot::builder().config(unreachable.clone()).secrets(secrets).build().unwrap();
    assert!(matches!(hj.resume("Me@Example.com").await, Err(Error::Transient(_))), "older session without a uid");

    let secrets = MemorySecrets::new();
    secrets.set("me@example.com", "refresh", "saved-refresh-token").unwrap();
    secrets.set("me@example.com", "uid", "uid-1").unwrap();
    let hj = HushJot::builder().config(unreachable).secrets(secrets).build().unwrap();
    let acct = hj.resume("Me@Example.com").await.unwrap().expect("resumed offline");
    assert_eq!(acct.uid(), "uid-1");
    assert_eq!(acct.email(), "me@example.com");
}

#[tokio::test]
async fn history_keeps_one_version_per_session_and_restores() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("draft 1", None).unwrap();
    a.vault.notes().update(&id, "draft 2").unwrap();
    a.vault.notes().update(&id, "draft 3").unwrap();
    a.vault.notes().update(&id, "draft 4").unwrap();

    let versions = a.vault.history(&id).list().await.unwrap();
    assert_eq!(versions.len(), 1, "one capture at the start of the session");
    let opened = a.vault.history(&id).open(&versions[0].id).await.unwrap();
    assert_eq!(opened.content, "draft 1");
    assert_eq!(opened.info.reason, VersionReason::Edit);

    a.vault.history(&id).restore(&versions[0].id).await.unwrap();
    assert_eq!(a.vault.notes().get(&id).unwrap().content, "draft 1");
    let after = a.vault.history(&id).list().await.unwrap();
    assert_eq!(after.len(), 2, "the replaced text was kept, so the restore can be undone");
    assert!(after.iter().any(|v| v.reason == VersionReason::Restore));

    // Uploaded versions are visible from another device.
    a.vault.sync_now().await;
    let b = device(&server).await;
    b.vault.sync_now().await;
    let remote = b.vault.history(&id).list().await.unwrap();
    assert_eq!(remote.len(), 2);
    assert!(remote.iter().all(|v| !v.local));
    assert_eq!(b.vault.history(&id).open(&remote[1].id).await.unwrap().content, "draft 1");
}

#[tokio::test]
async fn shares_publish_reopen_update_and_revoke() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("# Recipe\nflour, water", None).unwrap();
    let share = a.vault.shares().publish(&id, ShareExpiry::Day).await.unwrap();
    assert!(share.url.starts_with("https://hushjot.app/s/#s1."));

    // A recipient holding only the link can open it; the vault key is not needed.
    let r = parse_fragment(&share.url).unwrap();
    let payload = server.state().shares[&r.id].payload.clone();
    let opened = open_share(&import_share_key(&r.key).unwrap(), &payload).unwrap();
    assert_eq!(opened.body, "# Recipe\nflour, water");
    assert_eq!(opened.title, "Recipe");
    assert!(!support::server_saw(&server, &r.key), "the share key never reaches the server in the clear");

    a.vault.notes().update(&id, "# Recipe\nflour, water, salt").unwrap();
    let updated = a.vault.shares().update(&share.id, ShareExpiry::Week).await.unwrap();
    assert_eq!(updated.url, share.url, "re-publishing keeps the link");
    let payload = server.state().shares[&r.id].payload.clone();
    assert_eq!(open_share(&import_share_key(&r.key).unwrap(), &payload).unwrap().body, "# Recipe\nflour, water, salt");

    let listed = a.vault.shares().list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].url, share.url);

    a.vault.shares().revoke(&share.id).await.unwrap();
    assert!(server.state().shares.is_empty());
    assert!(a.vault.shares().list().await.unwrap().is_empty());
}

#[tokio::test]
async fn edits_that_change_nothing_are_not_writes() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("same", None).unwrap();
    a.vault.sync_now().await;
    assert!(!a.vault.notes().update(&id, "same").unwrap());
    a.vault.notes().set_pinned(&id, false).unwrap();
    a.vault.notes().move_to(&id, None).unwrap();
    assert_eq!(a.vault.pending_count(), 0);
}

#[tokio::test]
async fn pins_lead_the_list_and_carry_their_fields_across_edits() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let first = a.vault.notes().create("# First", None).unwrap();
    let second = a.vault.notes().create("# Second", None).unwrap();
    a.vault.notes().set_pinned(&first, true).unwrap();
    let ids: Vec<_> = a.vault.notes().list().unwrap().into_iter().map(|n| n.id).collect();
    assert_eq!(ids, [first.clone(), second]);

    let created = a.vault.notes().get(&first).unwrap().summary.created_at;
    a.vault.notes().update(&first, "# First, edited\nwith an excerpt").unwrap();
    let note = a.vault.notes().get(&first).unwrap();
    assert!(note.summary.pinned_at.is_some(), "an edit keeps the pin");
    assert_eq!(note.summary.created_at, created, "an edit keeps the created date");
    assert_eq!(note.summary.excerpt.as_deref(), Some("with an excerpt"));
}

#[tokio::test]
async fn missing_targets_are_reported() {
    let server = FakeServer::new();
    let a = device(&server).await;
    assert!(matches!(a.vault.notes().get("nope"), Err(Error::NotFound(_))));
    assert!(matches!(a.vault.notes().create("x", Some("no-folder")), Err(Error::NotFound(_))));
    assert!(matches!(a.vault.notes().update("nope", "x"), Err(Error::NotFound(_))));
    assert!(matches!(a.vault.folders().delete("nope"), Err(Error::NotFound(_))));
}

#[tokio::test]
async fn a_restarted_app_rebuilds_from_its_store() {
    let server = FakeServer::new();
    let store: Arc<dyn hushjot::Store> = Arc::new(MemoryStore::new());
    let id = {
        let a = device_with_store(&server, store.clone()).await;
        let id = a.vault.notes().create("# Persisted\nfind me", None).unwrap();
        a.vault.lock();
        id
    };
    let again = device_with_store(&server, store).await;
    assert_eq!(again.vault.notes().get(&id).unwrap().content, "# Persisted\nfind me");
    assert_eq!(again.vault.search("find", 5).unwrap().len(), 1, "the index is rebuilt, never stored");
    assert_eq!(again.vault.pending_count(), 1, "unsynced work is still queued");
}
