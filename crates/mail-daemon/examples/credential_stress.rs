//! Explicit private-bus diagnostic; never touches the desktop keyring.
use mail_service::credentials::{CredentialVault, SystemVault};
use std::path::PathBuf;
fn main() {
    let root = PathBuf::from(std::env::var_os("QINGMAIL_PERF_PRIVATE_ROOT").expect("private root"));
    assert!(root.is_absolute());
    assert_eq!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        format!("unix:path={}/bus", root.display())
    );
    let count: usize = std::env::args()
        .nth(1)
        .expect("reads per worker")
        .parse()
        .unwrap();
    assert!((1..=1000).contains(&count));
    let store = mail_storage::Store::open(&root.join("share/qingmail")).unwrap();
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 5);
    std::thread::scope(|scope| {
        for (id, _) in accounts {
            scope.spawn(move || {
                for _ in 0..count {
                    let value = SystemVault.load(&id).expect("private credential load");
                    assert_eq!(&*value.imap, "fixture-only");
                }
            });
        }
    });
    println!("credential_reads={} passed", count * 5);
}
