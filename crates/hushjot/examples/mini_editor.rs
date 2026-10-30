//! A tiny line-oriented "editor" that exercises everything an app integrates:
//! background sync and its events, conflicts, history, search, folders, share
//! links, the recovery code, locking, signing out and account resets.
//!
//! ```sh
//! HUSHJOT_EMAIL=me@example.com cargo run -p hushjot --example mini_editor
//! ```
//!
//! It asks for the password and passphrase on stdin (visibly — a real app uses
//! a password field). Type `help` for commands. Set `HUSHJOT_DATA_DIR` to run a
//! second copy against its own store, or a second account against this one.
//!
//! The walkthrough, with the SDK call behind each command, is
//! `docs/mini-editor.md` (https://github.com/hushjot/hushjot-rs/blob/main/docs/mini-editor.md).

use std::io::{BufRead, Write};

use hushjot::{
    Account, Error, FolderNode, HushJot, KeyringSecrets, ShareExpiry, SqliteStore, SyncEvent, SyncOptions, SyncStatus,
    UnlockOptions, Vault, PLANS_URL,
};

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok();
    line.trim_end_matches(['\r', '\n']).to_string()
}

/// `ask` off the async runtime, so background sync keeps running while we wait.
async fn ask_async(prompt: &str) -> String {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || ask(&prompt)).await.expect("stdin")
}

const HELP: &str = "\
notes
  ls                       list notes (pinned first)
  new <text>               create a note (use \\n for line breaks)
  big <kb>                 create a note of about <kb> KB (over 900 can't sync)
  cat <id>                 show a note
  append <id> <text>       add a line to a note
  pin <id> | unpin <id>    pin or unpin
  rm <id>                  delete (recoverable)
  trash                    list recently deleted notes, with full ids
  undelete <full id>       bring a deleted note back
  mv <id> <folder-id|/>    move a note
search
  search <query>           #tag, in:Folder, \"phrase\", -exclude
  tags                     list tags with counts
folders
  folders                  show the folder tree
  mkdir <name>             create a top-level folder
  rename-folder <id> <name>
  mvdir <id> <parent-id|/> move a folder (moving it inside itself is refused)
  rmdir <id>               delete a folder and everything in it
history
  history <id>             list versions
  restore <id> <ver>       restore a version
shares
  share <id>               publish a one-day link
  shares                   list live links
  reshare <share-id>       re-publish a link with the note's current text
  unshare <share-id>       revoke a link
account
  usage                    storage and push meter
  sync                     sync now
  forget-key               remove the cached vault key from the keychain
  lock                     drop the key from memory and ask for it again
  signout                  remove the session and cached key, then quit
  quit";

async fn run(vault: &Vault, account: &Account, line: &str) -> hushjot::Result<()> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match cmd {
        "help" => println!("{HELP}"),
        "ls" => {
            for n in vault.notes().list()? {
                let pin = if n.pinned_at.is_some() { "*" } else { " " };
                println!("{pin} {}  {}", &n.id[..8], n.title);
            }
        }
        "new" => println!("created {}", vault.notes().create(&rest.replace("\\n", "\n"), None)?),
        "big" => {
            let kb: usize = rest.parse().map_err(|_| Error::Invalid(format!("{rest:?} is not a number")))?;
            let text = format!("# Big note ({kb} KB)\n{}", "x".repeat(kb * 1024));
            println!("created {}", vault.notes().create(&text, None)?);
        }
        "cat" => println!("{}", vault.notes().get(&resolve(vault, rest)?)?.content),
        "append" => {
            let (id, text) = rest.split_once(' ').unwrap_or((rest, ""));
            let id = resolve(vault, id)?;
            let current = vault.notes().get(&id)?.content;
            vault.notes().update(&id, &format!("{}\n{text}", current.trim_end()))?;
        }
        "pin" => vault.notes().set_pinned(&resolve(vault, rest)?, true)?,
        "unpin" => vault.notes().set_pinned(&resolve(vault, rest)?, false)?,
        "rm" => vault.notes().delete(&resolve(vault, rest)?)?,
        "trash" => {
            for d in vault.notes().recently_deleted()? {
                println!("{}  {}  (deleted {})", d.id, d.title, when(d.deleted_at));
            }
        }
        "undelete" => vault.notes().undelete(rest).await?,
        "mv" => {
            let (id, folder) = rest.split_once(' ').unwrap_or((rest, "/"));
            let folder = if folder == "/" { None } else { Some(resolve_folder(vault, folder)?) };
            vault.notes().move_to(&resolve(vault, id)?, folder.as_deref())?;
        }
        "search" => {
            for hit in vault.search(rest, 20)? {
                println!("{:5.2}  {}  {}", hit.score, &hit.note.id[..8], hit.note.title);
            }
        }
        "tags" => {
            for tag in vault.tags() {
                println!("#{}  {}", tag.label, tag.count);
            }
        }
        "folders" => print_tree(&vault.folders().tree()?),
        "mkdir" => println!("folder {}", vault.folders().create(rest, None)?),
        "rename-folder" => {
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            vault.folders().rename(&resolve_folder(vault, id)?, name.trim())?;
        }
        "mvdir" => {
            let (id, parent) = rest.split_once(' ').unwrap_or((rest, "/"));
            let parent = if parent == "/" { None } else { Some(resolve_folder(vault, parent)?) };
            vault.folders().move_to(&resolve_folder(vault, id)?, parent.as_deref())?;
        }
        "rmdir" => {
            let id = resolve_folder(vault, rest)?;
            let (folders, notes) = vault.folders().contents(&id)?;
            let prompt = format!(
                "Delete {} with {folders} subfolder(s) and {notes} note(s)? [y/N] ",
                vault.folders().path(&id)?
            );
            if ask_async(&prompt).await == "y" {
                vault.folders().delete(&id)?;
            }
        }
        "history" => {
            for v in vault.history(resolve(vault, rest)?).list().await? {
                println!("{}  {:?}  at {}{}", v.id, v.reason, when(v.at), if v.local { "" } else { "  (server)" });
            }
        }
        "restore" => {
            let (id, version) = rest.split_once(' ').unwrap_or((rest, ""));
            vault.history(resolve(vault, id)?).restore(version).await?;
        }
        "share" => println!("{}", vault.shares().publish(&resolve(vault, rest)?, ShareExpiry::Day).await?.url),
        "shares" => {
            for s in vault.shares().list().await? {
                println!(
                    "{}  note {}  expires {}\n  {}",
                    s.id,
                    &s.note_id[..8.min(s.note_id.len())],
                    when(s.expire_at),
                    s.url
                );
            }
        }
        "reshare" => println!("{}", vault.shares().update(rest, ShareExpiry::Day).await?.url),
        "unshare" => vault.shares().revoke(rest).await?,
        "usage" => {
            let u = vault.usage().await?;
            println!("stored {} of {} bytes", u.bytes, u.quota);
            println!("pushes in the current window: {}", u.pushes_in_window);
            println!("this month: {} bytes of history, {} links", u.history_this_month, u.shares_this_month);
        }
        "sync" => println!("{:?}", vault.sync_now().await),
        "forget-key" => {
            account.forget_cached_key().await?;
            println!("Cached key removed. The next launch asks for the passphrase.");
        }
        "" => {}
        other => println!("unknown command {other:?}; try help"),
    }
    Ok(())
}

fn print_tree(nodes: &[FolderNode]) {
    for node in nodes {
        println!("{}{}  {}", "  ".repeat(node.depth), &node.folder.id[..8.min(node.folder.id.len())], node.folder.name);
        print_tree(&node.children);
    }
}

/// Local time for an epoch-ms timestamp.
fn when(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Accept a full id or an unambiguous prefix of one.
fn resolve(vault: &Vault, prefix: &str) -> hushjot::Result<String> {
    pick(vault.notes().list()?.into_iter().map(|n| n.id), prefix, "note")
}

/// The same, for folders.
fn resolve_folder(vault: &Vault, prefix: &str) -> hushjot::Result<String> {
    pick(vault.folders().list()?.into_iter().map(|f| f.id), prefix, "folder")
}

fn pick(ids: impl Iterator<Item = String>, prefix: &str, what: &str) -> hushjot::Result<String> {
    let matches: Vec<String> = ids.filter(|id| !prefix.is_empty() && id.starts_with(prefix)).collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::NotFound(format!("{what} {prefix}"))),
        _ => Err(Error::Invalid(format!("{prefix} matches more than one {what}"))),
    }
}

/// The passphrase screen: the cached key first (unless the user just locked),
/// then the passphrase or the recovery code, offering a wipe after a reset.
async fn unlock(hj: &HushJot, account: &Account, try_cached: bool) -> hushjot::Result<Vault> {
    if try_cached {
        match account.unlock_cached().await {
            Ok(Some(vault)) => return Ok(vault),
            Ok(None) | Err(Error::VaultReset) => {}
            Err(e) => return Err(e),
        }
    }
    loop {
        let secret = ask_async("passphrase (or type recovery): ").await;
        let recovery = secret == "recovery";
        let secret = if recovery { ask_async("recovery code: ").await } else { secret };
        let stay = ask_async("Stay unlocked on this device? [y/N] ").await == "y";
        let options = UnlockOptions { stay_unlocked: stay };
        let result = if recovery {
            account.unlock_with_recovery_code(&secret, options).await
        } else {
            account.unlock(&secret, options).await
        };
        match result {
            Ok(vault) => return Ok(vault),
            Err(Error::WrongSecret) if recovery => println!("That recovery code is not right."),
            Err(Error::WrongSecret) => println!("That passphrase is not right."),
            Err(Error::VaultReset) => {
                println!("This account was reset. This device's copy belongs to the old vault and can never sync.");
                if ask_async("Wipe this device's copy and try again? [y/N] ").await != "y" {
                    return Err(Error::VaultReset);
                }
                hj.wipe_local_data()?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// What the integration guide says to show for errors that end the session.
fn explain(e: &Error) {
    match e {
        Error::NoVault => println!("Set up HushJot once at hushjot.app first, then come back."),
        Error::NotApproved => println!("This account doesn't include HushJot Sync: {PLANS_URL}"),
        Error::Paused => println!("HushJot Sync is down for maintenance. Try again later."),
        Error::StoreBelongsToAnotherAccount => {
            println!("This store belongs to another account. Set HUSHJOT_DATA_DIR to use a separate one.")
        }
        _ => {}
    }
}

/// Print what sync reports, as an app would update its status bar.
fn watch(vault: &Vault) {
    let mut events = vault.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                SyncEvent::Changed { notes, .. } if !notes.is_empty() => {
                    println!("\n[sync] {} note(s) changed", notes.len())
                }
                SyncEvent::ConflictCopy { copy_id, .. } => println!("\n[sync] conflict kept as a copy: {copy_id}"),
                SyncEvent::Status(SyncStatus::Offline) => println!("\n[sync] offline; changes stay queued"),
                SyncEvent::NoteTooLarge { note_id } => {
                    println!("\n[sync] note {} is too large to sync (about 900 KB limit)", &note_id[..8])
                }
                SyncEvent::NotApproved => println!("\n[sync] this account needs a plan: {PLANS_URL}"),
                SyncEvent::Paused => println!("\n[sync] HushJot Sync is paused for maintenance"),
                SyncEvent::SessionExpired => println!("\n[sync] signed out; restart to sign in"),
                SyncEvent::StorageFull { waiting } if waiting > 0 => {
                    println!("\n[sync] storage full; {waiting} waiting")
                }
                SyncEvent::PushLimit { waiting, .. } if waiting > 0 => {
                    println!("\n[sync] daily limit; {waiting} waiting")
                }
                _ => {}
            }
        }
    });
}

enum Exit {
    Quit,
    Lock,
    SignOut,
}

#[tokio::main]
async fn main() -> hushjot::Result<()> {
    let email = std::env::var("HUSHJOT_EMAIL").unwrap_or_else(|_| ask("email: "));
    let dir = std::env::var_os("HUSHJOT_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hushjot-mini-editor"));
    std::fs::create_dir_all(&dir).expect("data dir");
    println!("store: {}", dir.join("store.db").display());
    let hj = HushJot::builder()
        .store(SqliteStore::open(dir.join("store.db"))?)
        .secrets(KeyringSecrets::new("com.hushjot.examples.mini-editor"))
        .build()?;

    let account = match hj.resume(&email).await? {
        Some(account) => account,
        None => hj.sign_in(&email, &ask("password: ")).await?,
    };

    let mut try_cached = true;
    loop {
        let vault = match unlock(&hj, &account, try_cached).await {
            Ok(vault) => vault,
            Err(e) => {
                explain(&e);
                return Err(e);
            }
        };
        watch(&vault);
        vault.start_sync(SyncOptions::default());
        println!("Unlocked {}. Type help.", vault.email());

        let exit = loop {
            let line = ask_async("> ").await;
            match line.as_str() {
                "quit" => break Exit::Quit,
                "lock" => break Exit::Lock,
                "signout" => break Exit::SignOut,
                _ => {}
            }
            if let Err(e) = run(&vault, &account, &line).await {
                println!("error: {e}");
            }
        };
        vault.sync_now().await;
        vault.lock();
        match exit {
            Exit::Quit => return Ok(()),
            Exit::Lock => {
                println!("Locked: the key is gone from memory. A cached key stays in the keychain until forget-key.");
                try_cached = false;
            }
            Exit::SignOut => {
                account.sign_out()?;
                println!("Signed out: the session and cached key are gone from the keychain.");
                return Ok(());
            }
        }
    }
}
