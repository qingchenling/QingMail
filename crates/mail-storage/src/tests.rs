use super::*;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    process::Command,
};

pub(super) fn settings() -> AccountSettings {
    AccountSettings {
        display_name: "个人邮箱".into(),
        address: "tester@example.org".into(),
        imap: ServerSettings {
            host: "imap.example.org".into(),
            port: 993,
            username: "imap-user".into(),
            security: ConnectionSecurity::Tls,
        },
        smtp: ServerSettings {
            host: "smtp.example.org".into(),
            port: 587,
            username: "smtp-user".into(),
            security: ConnectionSecurity::StartTls,
        },
    }
}
fn envelope() -> MessageEnvelope {
    MessageEnvelope {
        subject: "中文标题🙂".into(),
        sender: "陈雨 <test@example.org>".into(),
        received_at: 1794787200,
    }
}
fn location(folder: &Folder, uid: u32) -> MailUid {
    MailUid {
        account: folder.account.clone(),
        folder_id: folder.id.0.clone(),
        uidvalidity: folder.uidvalidity,
        uid,
    }
}

#[test]
fn reopen_preserves_ids_distinct_accounts_and_keyset_pages() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("qingmail");
    let mut store = Store::open(&path).unwrap();
    let a = store.create_account(&settings()).unwrap();
    let b = store.create_account(&settings()).unwrap();
    assert_ne!(a, b);
    let folder = store.upsert_folder(&a, "INBOX", 42).unwrap();
    let other = store.upsert_folder(&b, "INBOX", 42).unwrap();
    let first = store
        .cache_message(&location(&folder, 1), &envelope())
        .unwrap();
    assert_eq!(
        first,
        store
            .cache_message(&location(&folder, 1), &envelope())
            .unwrap()
    );
    let second = store
        .cache_message(&location(&other, 1), &envelope())
        .unwrap();
    assert_ne!(first, second);
    for uid in [2, 3, u32::MAX] {
        store
            .cache_message(&location(&folder, uid), &envelope())
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.account(&a).unwrap(), Some(settings()));
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts.contains(&(a.clone(), settings())));
    assert!(accounts.contains(&(b.clone(), settings())));
    assert_eq!(store.folders(&a).unwrap(), vec![folder.clone()]);
    let page = store.list_messages(&folder, None, 2).unwrap();
    assert_eq!(
        page.iter().map(|x| x.location.uid).collect::<Vec<_>>(),
        vec![u32::MAX, 3]
    );
    let page = store.list_messages(&folder, Some(3), 2).unwrap();
    assert_eq!(
        page.iter().map(|x| x.location.uid).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(page[1].id, first);
    assert_eq!(page[1].envelope, envelope());
    assert_eq!(store.list_messages(&other, None, 20).unwrap()[0].id, second);
    assert!(store.list_messages(&folder, None, 201).is_err());
    assert!(store.list_messages(&folder, None, 0).is_err());
}

#[test]
fn epoch_change_invalidates_only_affected_folder_and_rejects_stale_writes() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(&root.path().join("qingmail")).unwrap();
    let a = store.create_account(&settings()).unwrap();
    let inbox = store.upsert_folder(&a, "INBOX", 42).unwrap();
    let archive = store.upsert_folder(&a, "Archive", 42).unwrap();
    let old = store
        .cache_message(&location(&inbox, 7), &envelope())
        .unwrap();
    store
        .cache_message(&location(&archive, 7), &envelope())
        .unwrap();
    assert_eq!(store.upsert_folder(&a, "INBOX", 42).unwrap(), inbox);
    assert_eq!(store.list_messages(&inbox, None, 20).unwrap().len(), 1);
    let new = store.upsert_folder(&a, "INBOX", 43).unwrap();
    assert_eq!(new.id, inbox.id);
    assert!(store.list_messages(&inbox, None, 20).unwrap().is_empty());
    assert!(store.list_messages(&new, None, 20).unwrap().is_empty());
    assert_eq!(store.list_messages(&archive, None, 20).unwrap().len(), 1);
    let count: u32 = store
        .connection
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert!(
        store
            .cache_message(&location(&inbox, 8), &envelope())
            .is_err()
    );
    assert_eq!(
        count,
        store
            .connection
            .query_row::<u32, _, _>("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    );
    assert_ne!(
        old,
        store
            .cache_message(&location(&new, 7), &envelope())
            .unwrap()
    );
}

#[test]
fn relational_constraints_allow_multiple_folders_but_not_cross_account_links() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(&root.path().join("qingmail")).unwrap();
    let a = store.create_account(&settings()).unwrap();
    let b = store.create_account(&settings()).unwrap();
    let inbox = store.upsert_folder(&a, "INBOX", 1).unwrap();
    let archive = store.upsert_folder(&a, "Archive", 2).unwrap();
    let foreign = store.upsert_folder(&b, "INBOX", 1).unwrap();
    let id = store
        .cache_message(&location(&inbox, 1), &envelope())
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO folder_messages(account_id,folder_id,uidvalidity,uid,message_id,seen,flagged) VALUES (?1,?2,2,1,?3,0,0)",
            params![a.0, archive.id.0, id.0],
        )
        .unwrap();
    assert_eq!(store.list_messages(&archive, None, 20).unwrap()[0].id, id);
    assert!(
        store
            .connection
            .execute(
                "INSERT INTO folder_messages(account_id,folder_id,uidvalidity,uid,message_id,seen,flagged) VALUES (?1,?2,1,1,?3,0,0)",
                params![b.0, foreign.id.0, id.0]
            )
            .is_err()
    );
    assert!(
        store
            .cache_message(&location(&inbox, 0), &envelope())
            .is_err()
    );
    assert!(store.upsert_folder(&a, "INBOX", 0).is_err());
}

#[test]
fn private_permissions_lock_and_symlink_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("qingmail");
    let store = Store::open(&path).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
    for name in ["state.sqlite3", "writer.lock"] {
        assert_eq!(fs::metadata(path.join(name)).unwrap().mode() & 0o777, 0o600);
    }
    drop(store);
    let alias = root.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(Store::open(&alias).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let db = path.join("state.sqlite3");
    fs::rename(&db, path.join("original")).unwrap();
    symlink(path.join("original"), &db).unwrap();
    assert!(Store::open(&path).is_err());
    fs::remove_file(&db).unwrap();
    fs::hard_link(path.join("original"), &db).unwrap();
    assert!(Store::open(&path).is_err());
    fs::remove_file(&db).unwrap();
    fs::rename(path.join("original"), &db).unwrap();
    symlink(&db, path.join("state.sqlite3-journal")).unwrap();
    assert!(Store::open(&path).is_err());
}

#[test]
fn future_foreign_and_corrupt_databases_are_preserved() {
    for kind in ["future", "foreign", "corrupt"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("qingmail");
        drop(Store::open(&path).unwrap());
        let db = path.join("state.sqlite3");
        if kind == "corrupt" {
            fs::write(&db, b"broken database, do not overwrite").unwrap();
        } else {
            let conn = Connection::open(&db).unwrap();
            if kind == "future" {
                conn.pragma_update(None, "user_version", 999).unwrap();
            } else {
                conn.pragma_update(None, "application_id", 123).unwrap();
            }
        }
        let before = fs::read(&db).unwrap();
        assert!(Store::open(&path).is_err());
        assert_eq!(fs::read(&db).unwrap(), before);
    }
}

#[test]
fn migration_failure_rolls_back_and_keeps_private_backup() {
    let root = tempfile::tempdir().unwrap();
    let directory = PrivateDir::open(&root.path().join("qingmail")).unwrap();
    let _writer = directory.lock().unwrap();
    let _file = directory.file("state.sqlite3", false).unwrap();
    let mut conn = Connection::open(directory.path("state.sqlite3")).unwrap();
    conn.execute_batch("CREATE TABLE temporary_marker(id INTEGER); DROP TABLE temporary_marker;")
        .unwrap();
    assert!(
        migrate(
            &mut conn,
            &directory,
            true,
            "CREATE TABLE half_done(id INTEGER); INVALID SQL;",
            SCHEMA_V2,
            SCHEMA_V3
        )
        .is_err()
    );
    assert_eq!(validate_schema(&conn).unwrap(), 0);
    let backups: Vec<_> = fs::read_dir(directory.path(""))
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            e.file_name()
                .to_string_lossy()
                .starts_with("migration-v0-")
                .then_some(e.path())
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::metadata(&backups[0]).unwrap().mode() & 0o777, 0o600);
    let backup = Connection::open(&backups[0]).unwrap();
    assert_eq!(validate_schema(&backup).unwrap(), 0);
    migrate(&mut conn, &directory, true, SCHEMA, SCHEMA_V2, SCHEMA_V3).unwrap();
    assert_eq!(validate_schema(&conn).unwrap(), SCHEMA_VERSION);
}

#[test]
fn abrupt_process_exit_preserves_committed_data_and_rolls_back_pending_transaction() {
    const KEY: &str = "QINGMAIL_STORAGE_CRASH_TEST";
    if let Some(path) = std::env::var_os(KEY) {
        let store = Store::open(Path::new(&path)).unwrap();
        store
            .connection
            .execute_batch("BEGIN IMMEDIATE; UPDATE accounts SET display_name='未提交';")
            .unwrap();
        std::process::exit(73);
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("qingmail");
    let mut store = Store::open(&path).unwrap();
    let account = store.create_account(&settings()).unwrap();
    drop(store);
    let status=Command::new(std::env::current_exe().unwrap())
        .args(["--exact","tests::abrupt_process_exit_preserves_committed_data_and_rolls_back_pending_transaction"])
        .env(KEY,&path).status().unwrap();
    assert_eq!(status.code(), Some(73));
    let store = Store::open(&path).unwrap();
    assert_eq!(store.account(&account).unwrap(), Some(settings()));
}

#[test]
fn failed_write_keeps_last_committed_account() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("qingmail");
    let mut store = Store::open(&path).unwrap();
    let account = store.create_account(&settings()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    let failed = store.create_account(&settings());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(failed.is_err());
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.accounts().unwrap(), vec![(account, settings())]);
}
