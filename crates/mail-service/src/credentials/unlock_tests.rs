//! Private D-Bus provider verifies the real Unlock -> Prompt -> Completed flow.
use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::AtomicUsize},
};
use zbus::{
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const ROOT: &str = "/org/freedesktop/secrets";
const COLLECTION: &str = "/org/freedesktop/secrets/collection/default";
const PROMPT: &str = "/org/freedesktop/secrets/prompt/unlock";
fn path(value: &str) -> OwnedObjectPath {
    value.try_into().unwrap()
}

#[derive(Default)]
struct State {
    locked: AtomicBool,
    dismiss: AtomicBool,
    unchanged: AtomicBool,
    prompts: AtomicUsize,
}
struct Service;
#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    fn open_session(&self, algorithm: &str, _input: Value<'_>) -> (OwnedValue, OwnedObjectPath) {
        assert_eq!(algorithm, "plain");
        (
            Value::from("").try_to_owned().unwrap(),
            path("/org/freedesktop/secrets/session/test"),
        )
    }
    fn read_alias(&self, alias: &str) -> OwnedObjectPath {
        assert_eq!(alias, "default");
        path(COLLECTION)
    }
    fn unlock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        assert_eq!(objects, vec![path(COLLECTION)]);
        (vec![], path(PROMPT))
    }
}
struct Collection(Arc<State>);
#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    #[zbus(property)]
    fn locked(&self) -> bool {
        self.0.locked.load(Ordering::Acquire)
    }
}
struct Prompt(Arc<State>);
#[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    async fn prompt(
        &self,
        _window_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.0.prompts.fetch_add(1, Ordering::AcqRel);
        let dismiss = self.0.dismiss.load(Ordering::Acquire);
        if !dismiss && !self.0.unchanged.load(Ordering::Acquire) {
            self.0.locked.store(false, Ordering::Release);
        }
        Self::completed(&emitter, dismiss, Value::from(vec![path(COLLECTION)]))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Value<'_>,
    ) -> zbus::Result<()>;
}
struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn system_unlock_prompts_only_when_locked_and_verifies_completion() {
    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = Bus(child);
    assert!(!address.trim().is_empty(), "private D-Bus did not start");
    let state = Arc::new(State::default());
    futures_lite::future::block_on(async {
        let _provider = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.freedesktop.secrets")
            .unwrap()
            .serve_at(ROOT, Service)
            .unwrap()
            .serve_at(COLLECTION, Collection(state.clone()))
            .unwrap()
            .serve_at(PROMPT, Prompt(state.clone()))
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .method_timeout(Duration::from_secs(2))
            .build()
            .await
            .unwrap();
        let service = SecretService::connect_with_existing(EncryptionType::Plain, connection)
            .await
            .unwrap();

        assert_eq!(unlock_default(&service).await, Ok(false));
        assert_eq!(state.prompts.load(Ordering::Acquire), 0);
        state.locked.store(true, Ordering::Release);
        assert_eq!(unlock_default(&service).await, Ok(true));
        assert_eq!(state.prompts.load(Ordering::Acquire), 1);
        assert!(!state.locked.load(Ordering::Acquire));

        state.locked.store(true, Ordering::Release);
        state.dismiss.store(true, Ordering::Release);
        assert_eq!(unlock_default(&service).await, Err(VaultError::Locked));
        assert!(state.locked.load(Ordering::Acquire));
        state.dismiss.store(false, Ordering::Release);
        state.unchanged.store(true, Ordering::Release);
        assert_eq!(unlock_default(&service).await, Err(VaultError::Locked));
        assert_eq!(state.prompts.load(Ordering::Acquire), 3);
    });
}

#[test]
fn closing_cancels_unlock_wait_without_waiting_for_prompt_timeout() {
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            cancel.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        assert_eq!(
            wait_for_unlock(std::future::pending::<Result<bool, VaultError>>(), &cancel),
            Err(VaultError::Locked)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    });
    assert_eq!(
        wait_for_unlock::<()>(
            async {
                panic!("cancelled request must not run");
            },
            &cancel
        ),
        Err(VaultError::Locked)
    );
}
