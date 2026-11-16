//! 仅在 dbus-run-session 私有会话里运行，不替换用户桌面 watcher。
use mail_daemon::{Command, Tray};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
};
use zbus::{
    blocking::{Proxy, connection::Builder},
    zvariant::{OwnedValue, Value},
};

#[derive(Default)]
struct State {
    host: bool,
    items: Vec<String>,
}
struct Watcher(Arc<Mutex<State>>);
#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.0.lock().unwrap().items.push(service.into());
    }
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        self.0.lock().unwrap().host
    }
}
fn main() {
    assert_eq!(
        std::env::var("QINGMAIL_TEST_PRIVATE_BUS").as_deref(),
        Ok("1"),
        "Use a private dbus-run-session"
    );
    let (tx, rx) = mpsc::sync_channel(8);
    assert!(
        Tray::start(tx.clone()).is_err(),
        "missing watcher must not claim a tray"
    );
    let state = Arc::new(Mutex::new(State::default()));
    let bus = Builder::session()
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at("/StatusNotifierWatcher", Watcher(state.clone()))
        .unwrap()
        .build()
        .unwrap();
    assert!(
        Tray::start(tx.clone()).is_err(),
        "watcher with no host must not claim availability"
    );
    state.lock().unwrap().host = true;
    let tray = Tray::start(tx).unwrap();
    let name = state.lock().unwrap().items.last().unwrap().clone();
    let item = Proxy::new(
        &bus,
        name.as_str(),
        "/StatusNotifierItem",
        "org.kde.StatusNotifierItem",
    )
    .unwrap();
    assert_eq!(item.get_property::<String>("Status").unwrap(), "Active");
    assert_eq!(
        item.get_property::<String>("Category").unwrap(),
        "Communications"
    );
    let pixmap: Vec<(i32, i32, Vec<u8>)> = item.get_property("IconPixmap").unwrap();
    assert_eq!(pixmap[0].2.len(), (pixmap[0].0 * pixmap[0].1 * 4) as usize);
    let menu = Proxy::new(&bus, name.as_str(), "/Menu", "com.canonical.dbusmenu").unwrap();
    type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
    let (_, layout): (u32, Layout) = menu
        .call("GetLayout", &(0_i32, -1_i32, Vec::<String>::new()))
        .unwrap();
    assert_eq!(layout.2.len(), 2);
    item.call::<_, _, ()>("Activate", &(0_i32, 0_i32)).unwrap();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        Command::Open
    );
    menu.call::<_, _, ()>("Event", &(1_i32, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        Command::Open
    );
    menu.call::<_, _, ()>("Event", &(2_i32, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        Command::Quit
    );
    drop(tray);
    println!(
        "PASS: missing watcher; missing host; registered item properties/icon; menu layout; activate/open; menu quit (6 checks, private session bus)"
    );
}
