//! Sync scenarios: two or more devices against one server that enforces the
//! real rules. Each test is a story an editor's users will live through.

mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use hushjot::core::usage::DAILY_PUSH_LIMIT;
use hushjot::{SyncEvent, SyncStatus};
use support::{device, server_saw, FakeServer};

#[tokio::test]
async fn a_note_written_on_one_device_arrives_on_another() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;

    let folder = a.vault.folders().create("Work", None).unwrap();
    let id = a.vault.notes().create("# Standup\nShip the SDK", Some(&folder)).unwrap();
    assert_eq!(a.vault.pending_count(), 2);
    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);

    assert_eq!(b.vault.sync_now().await, SyncStatus::Idle);
    let note = b.vault.notes().get(&id).unwrap();
    assert_eq!(note.content, "# Standup\nShip the SDK");
    assert_eq!(note.summary.title, "Standup");
    assert_eq!(note.summary.excerpt.as_deref(), Some("Ship the SDK"));
    assert_eq!(note.summary.folder_id.as_deref(), Some(folder.as_str()));
    assert_eq!(b.vault.folders().list().unwrap()[0].name, "Work");
}

#[tokio::test]
async fn the_server_never_sees_plaintext() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let folder = a.vault.folders().create("SecretFolderName", None).unwrap();
    let id = a.vault.notes().create("# SecretTitle\nSecretBodyText", Some(&folder)).unwrap();
    a.vault.notes().update(&id, "# SecretTitle\nSecretBodyText edited").unwrap();
    a.vault.notes().delete(&id).unwrap();
    a.vault.sync_now().await;
    assert!(!server.all_stored_bytes().is_empty());
    for needle in ["SecretFolderName", "SecretTitle", "SecretBodyText"] {
        assert!(!server_saw(&server, needle), "{needle} reached the server in the clear");
    }
}

#[tokio::test]
async fn every_push_sends_the_server_revision_plus_one_not_the_local_counter() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("one", None).unwrap();
    for text in ["two", "three", "four", "five"] {
        a.vault.notes().update(&id, text).unwrap();
    }
    a.vault.sync_now().await;
    assert_eq!(server.state().notes[&id].revision, 1, "five local writes, one accepted push");
    a.vault.notes().update(&id, "six").unwrap();
    a.vault.notes().update(&id, "seven").unwrap();
    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);
    assert_eq!(server.state().notes[&id].revision, 2);
}

#[tokio::test]
async fn concurrent_edits_keep_both_texts_as_a_conflict_copy() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let id = a.vault.notes().create("# Plan\nbase", None).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;

    a.vault.notes().update(&id, "# Plan\nfrom A").unwrap();
    b.vault.notes().update(&id, "# Plan\nfrom B").unwrap();
    a.vault.sync_now().await;

    let mut events = b.vault.subscribe();
    b.vault.sync_now().await;

    assert_eq!(b.vault.notes().get(&id).unwrap().content, "# Plan\nfrom A", "the server's version keeps the id");
    let copy = b.vault.notes().list().unwrap().into_iter().find(|n| n.id != id).expect("a conflict copy");
    assert!(copy.title.starts_with("Plan (conflict copy "), "{}", copy.title);
    assert_eq!(b.vault.notes().get(&copy.id).unwrap().content, "# Plan\nfrom B");

    let mut saw_copy_event = false;
    while let Ok(event) = events.try_recv() {
        if let SyncEvent::ConflictCopy { note_id, copy_id } = event {
            assert_eq!((note_id.as_str(), copy_id.as_str()), (id.as_str(), copy.id.as_str()));
            saw_copy_event = true;
        }
    }
    assert!(saw_copy_event);

    // The copy reaches the other device too, and nothing is left queued.
    b.vault.sync_now().await;
    a.vault.sync_now().await;
    assert_eq!(a.vault.notes().list().unwrap().len(), 2);
    assert_eq!(b.vault.pending_count(), 0);
}

#[tokio::test]
async fn identical_concurrent_edits_make_no_copy() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let id = a.vault.notes().create("base", None).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;
    a.vault.notes().update(&id, "same edit").unwrap();
    b.vault.notes().update(&id, "same edit").unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;
    assert_eq!(b.vault.notes().list().unwrap().len(), 1);
}

#[tokio::test]
async fn a_pin_racing_an_edit_does_not_fork_the_note() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let id = a.vault.notes().create("# Shared\nv1", None).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;

    a.vault.notes().update(&id, "# Shared\nv2 from A").unwrap();
    b.vault.notes().set_pinned(&id, true).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;

    let notes = b.vault.notes().list().unwrap();
    assert_eq!(notes.len(), 1, "metadata-only change must rebase, not copy");
    assert!(notes[0].pinned_at.is_some(), "the local pin survives");
    assert_eq!(b.vault.notes().get(&id).unwrap().content, "# Shared\nv2 from A", "the remote text is adopted");

    a.vault.sync_now().await;
    let on_a = a.vault.notes().get(&id).unwrap();
    assert!(on_a.summary.pinned_at.is_some());
    assert_eq!(on_a.content, "# Shared\nv2 from A");
}

#[tokio::test]
async fn a_lost_acknowledgement_is_recognised_not_forked() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("first", None).unwrap();
    a.vault.sync_now().await;
    a.vault.notes().update(&id, "second").unwrap();

    server.drop_next_ack.store(true, Ordering::SeqCst);
    assert_eq!(a.vault.sync_now().await, SyncStatus::Offline);
    assert_eq!(a.vault.pending_count(), 1);

    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);
    assert_eq!(a.vault.notes().list().unwrap().len(), 1, "no conflict copy over a dropped packet");
    assert_eq!(server.state().notes[&id].revision, 2);
}

#[tokio::test]
async fn deletes_propagate_and_keep_the_text_for_undelete() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let id = a.vault.notes().create("# Gone soon\nlast words", None).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;

    a.vault.notes().delete(&id).unwrap();
    a.vault.sync_now().await;
    assert!(server.state().notes[&id].deleted);
    assert!(server.state().notes[&id].body.is_none(), "a tombstone has no body");
    assert_eq!(server.state().versions.len(), 1, "the final text went up before the tombstone");

    b.vault.sync_now().await;
    assert!(b.vault.notes().list().unwrap().is_empty());
    assert!(b.vault.notes().get(&id).is_err());
    assert_eq!(b.vault.notes().recently_deleted().unwrap()[0].title, "Gone soon");

    b.vault.notes().undelete(&id).await.unwrap();
    assert_eq!(b.vault.notes().get(&id).unwrap().content, "# Gone soon\nlast words");
    b.vault.sync_now().await;
    a.vault.sync_now().await;
    assert_eq!(a.vault.notes().get(&id).unwrap().content, "# Gone soon\nlast words");
}

#[tokio::test]
async fn undelete_fetches_the_text_from_the_server_when_this_device_has_none() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let id = a.vault.notes().create("# Kept\ntext", None).unwrap();
    a.vault.sync_now().await;
    a.vault.notes().delete(&id).unwrap();
    a.vault.sync_now().await;

    // A device that never had the note at all.
    let c = device(&server).await;
    c.vault.sync_now().await;
    c.vault.notes().undelete(&id).await.unwrap();
    assert_eq!(c.vault.notes().get(&id).unwrap().content, "# Kept\ntext");
}

#[tokio::test]
async fn folder_changes_sync_and_the_later_write_wins() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let work = a.vault.folders().create("Work", None).unwrap();
    let db = a.vault.folders().create("DB", Some(&work)).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;
    assert_eq!(b.vault.folders().path(&db).unwrap(), "Work / DB");

    a.vault.folders().rename(&work, "Job").unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;
    assert_eq!(b.vault.folders().path(&db).unwrap(), "Job / DB");

    b.vault.folders().move_to(&db, None).unwrap();
    b.vault.sync_now().await;
    a.vault.sync_now().await;
    assert_eq!(a.vault.folders().path(&db).unwrap(), "DB");
}

#[tokio::test]
async fn deleting_a_folder_deletes_its_subtree_everywhere() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let work = a.vault.folders().create("Work", None).unwrap();
    let db = a.vault.folders().create("DB", Some(&work)).unwrap();
    a.vault.notes().create("in work", Some(&work)).unwrap();
    a.vault.notes().create("in db", Some(&db)).unwrap();
    let kept = a.vault.notes().create("top level", None).unwrap();
    assert_eq!(a.vault.folders().contents(&work).unwrap(), (1, 2));

    a.vault.folders().delete(&work).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;
    assert!(b.vault.folders().list().unwrap().is_empty());
    let left: Vec<_> = b.vault.notes().list().unwrap().into_iter().map(|n| n.id).collect();
    assert_eq!(left, [kept]);
}

#[tokio::test]
async fn a_move_into_its_own_subtree_is_refused() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let work = a.vault.folders().create("Work", None).unwrap();
    let db = a.vault.folders().create("DB", Some(&work)).unwrap();
    assert!(a.vault.folders().move_to(&work, Some(&db)).is_err());
    assert!(a.vault.folders().rename(&work, "   ").is_err());
    assert_eq!(
        a.vault.folders().create("  ", None).map(|id| a.vault.folders().path(&id).unwrap()).unwrap(),
        "New folder"
    );
}

#[tokio::test]
async fn large_libraries_push_and_pull_in_pages() {
    let server = FakeServer::new();
    let a = device(&server).await;
    for i in 0..450 {
        a.vault.notes().create(&format!("note {i}"), None).unwrap();
    }
    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);
    assert_eq!(server.state().notes.len(), 450);
    let b = device(&server).await;
    b.vault.sync_now().await;
    assert_eq!(b.vault.notes().list().unwrap().len(), 450);
}

#[tokio::test]
async fn a_full_push_window_holds_changes_until_it_rolls_over() {
    let server = FakeServer::new();
    let a = device(&server).await;
    {
        let mut s = server.state();
        s.pn = DAILY_PUSH_LIMIT;
        s.ws = Some(server.now());
    }
    let mut events = a.vault.subscribe();
    let id = a.vault.notes().create("held", None).unwrap();
    a.vault.sync_now().await;
    assert!(!server.state().notes.contains_key(&id));
    assert_eq!(a.vault.pending_count(), 1);
    let mut saw = false;
    while let Ok(e) = events.try_recv() {
        if let SyncEvent::PushLimit { waiting, .. } = e {
            saw |= waiting == 1;
        }
    }
    assert!(saw, "the app is told how much is waiting");

    // A day later by the server's clock, a reset is accepted.
    server.advance_clock(25 * 60 * 60 * 1000);
    let fresh = support::device(&server).await;
    drop(fresh);
    a.vault.sync_now().await;
    assert!(server.state().notes.contains_key(&id));
    assert_eq!(server.state().pn, 1);
}

#[tokio::test]
async fn storage_full_holds_growth_and_reports_it() {
    let server = FakeServer::new();
    let a = device(&server).await;
    server.state().quota = 1024;
    a.vault.notes().create("fits", None).unwrap();
    a.vault.sync_now().await;
    let mut events = a.vault.subscribe();
    let id = a.vault.notes().create("does not fit", None).unwrap();
    a.vault.sync_now().await;
    assert!(!server.state().notes.contains_key(&id));
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|e| e == SyncEvent::StorageFull { waiting: 1 }));
    assert_eq!(a.vault.notes().get(&id).unwrap().content, "does not fit", "still editable locally");
}

#[tokio::test]
async fn an_account_without_a_plan_is_reported_and_keeps_working_locally() {
    let server = FakeServer::new();
    let a = device(&server).await;
    server.approved.store(false, Ordering::SeqCst);
    let mut events = a.vault.subscribe();
    let id = a.vault.notes().create("offline-ish", None).unwrap();
    assert_eq!(a.vault.sync_now().await, SyncStatus::NotApproved);
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|e| e == SyncEvent::NotApproved));
    a.vault.notes().update(&id, "still editable").unwrap();
    assert_eq!(a.vault.pending_count(), 1);
}

#[tokio::test]
async fn a_maintenance_pause_is_not_reported_as_a_lapsed_plan() {
    let server = FakeServer::new();
    let a = device(&server).await;
    a.vault.notes().create("x", None).unwrap();
    server.paused.store(true, Ordering::SeqCst);
    let mut events = a.vault.subscribe();
    assert_eq!(a.vault.sync_now().await, SyncStatus::Paused);
    let seen: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert!(seen.contains(&SyncEvent::Paused));
    assert!(!seen.contains(&SyncEvent::NotApproved));

    server.paused.store(false, Ordering::SeqCst);
    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);
}

#[tokio::test]
async fn an_expired_session_stops_sync_and_says_so() {
    let server = FakeServer::new();
    let a = device(&server).await;
    a.vault.notes().create("x", None).unwrap();
    server.session_expired.store(true, Ordering::SeqCst);
    assert_eq!(a.vault.sync_now().await, SyncStatus::SignedOut);
    assert_eq!(a.vault.pending_count(), 1);
}

#[tokio::test]
async fn offline_work_survives_and_syncs_when_the_network_returns() {
    let server = FakeServer::new();
    let a = device(&server).await;
    server.offline.store(true, Ordering::SeqCst);
    let id = a.vault.notes().create("written on a plane", None).unwrap();
    a.vault.notes().update(&id, "written on a plane, edited").unwrap();
    assert_eq!(a.vault.sync_now().await, SyncStatus::Offline);
    server.offline.store(false, Ordering::SeqCst);
    assert_eq!(a.vault.sync_now().await, SyncStatus::Idle);
    let b = device(&server).await;
    b.vault.sync_now().await;
    assert_eq!(b.vault.notes().get(&id).unwrap().content, "written on a plane, edited");
}

#[tokio::test]
async fn remote_changes_reach_the_search_index() {
    let server = FakeServer::new();
    let a = device(&server).await;
    let b = device(&server).await;
    let folder = a.vault.folders().create("Databases", None).unwrap();
    a.vault.notes().create("# Postgres tuning\nconnection pool size #db", Some(&folder)).unwrap();
    a.vault.notes().create("# Shopping\noat milk", None).unwrap();
    a.vault.sync_now().await;
    b.vault.sync_now().await;

    let hits = b.vault.search("postgres", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].in_title);
    assert_eq!(b.vault.search("#db", 10).unwrap().len(), 1);
    assert_eq!(b.vault.search("in:datab pool", 10).unwrap().len(), 1);
    assert_eq!(b.vault.search("in:/ milk", 10).unwrap().len(), 1);
    assert_eq!(b.vault.search("\"pool size\"", 10).unwrap().len(), 1);
    assert!(b.vault.search("\"size pool\"", 10).unwrap().is_empty());
    assert!(b.vault.search("-postgres", 10).unwrap().is_empty());
    assert_eq!(b.vault.tags()[0].key, "db");
}

#[tokio::test]
async fn background_sync_pushes_on_its_own_cadence() {
    let server = FakeServer::new();
    let a = device(&server).await;
    a.vault.start_sync(hushjot::SyncOptions {
        nudge: std::time::Duration::from_millis(10),
        idle: std::time::Duration::from_millis(20),
        ..Default::default()
    });
    let id = a.vault.notes().create("by itself", None).unwrap();
    let mut status = a.vault.status();
    for _ in 0..200 {
        if server.state().notes.contains_key(&id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(server.state().notes.contains_key(&id), "the nudge pushed a structural change");
    a.vault.notes().update(&id, "typed later").unwrap();
    for _ in 0..200 {
        if server.state().notes[&id].revision == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(server.state().notes[&id].revision, 2, "the idle debounce pushed an edit");
    let _ = status.changed().await;
    a.vault.stop_sync();
}

#[tokio::test]
async fn many_devices_converge() {
    let server = FakeServer::new();
    let devices = futures_join_all(&server, 4).await;
    for (i, d) in devices.iter().enumerate() {
        d.vault.notes().create(&format!("# From device {i}"), None).unwrap();
    }
    for _ in 0..2 {
        for d in &devices {
            d.vault.sync_now().await;
        }
    }
    for d in &devices {
        assert_eq!(d.vault.notes().list().unwrap().len(), 4);
        assert_eq!(d.vault.pending_count(), 0);
    }
}

async fn futures_join_all(server: &Arc<FakeServer>, n: usize) -> Vec<support::Device> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(device(server).await);
    }
    out
}
