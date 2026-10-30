//! Sign in, unlock, list, create, sync.
//!
//! ```sh
//! HUSHJOT_EMAIL=me@example.com HUSHJOT_PASSWORD=… HUSHJOT_PASSPHRASE=… \
//!   cargo run -p hushjot --example quickstart
//! ```
//!
//! Environment variables keep the example short. A real app asks for the
//! password and passphrase in its own UI and never logs them.

use hushjot::{HushJot, SqliteStore, SyncStatus, UnlockOptions};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

#[tokio::main]
async fn main() -> hushjot::Result<()> {
    let data_dir = std::env::temp_dir().join("hushjot-quickstart");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let hj = HushJot::builder().store(SqliteStore::open(data_dir.join("hushjot.db"))?).build()?;

    let account = hj.sign_in(&env("HUSHJOT_EMAIL"), &env("HUSHJOT_PASSWORD")).await?;
    let vault = account.unlock(&env("HUSHJOT_PASSPHRASE"), UnlockOptions::default()).await?;

    // Pull what the account already holds.
    match vault.sync_now().await {
        SyncStatus::Idle => {}
        other => eprintln!("sync: {other:?}"),
    }

    for note in vault.notes().list()? {
        let pin = if note.pinned_at.is_some() { "📌 " } else { "" };
        println!("{pin}{}  ({})", note.title, note.id);
    }

    let id = vault.notes().create("# Hello from Rust\nWritten with the hushjot crate.", None)?;
    println!("created {id}");
    println!("sync: {:?}", vault.sync_now().await);
    Ok(())
}
