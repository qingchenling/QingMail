//! Browser authorization is asynchronous. Only a short-lived Secret Service reference crosses IPC.
use mail_protocol::{ErrorCode, Reply, RpcError};
use mail_service::{
    credentials::{Credentials, SystemVault, staging::CredentialStaging},
    microsoft::Login,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct MicrosoftLogin {
    entry: Mutex<Option<Arc<Entry>>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}
struct Entry {
    id: String,
    url: String,
    cancel: AtomicBool,
    expires: Instant,
    result: Mutex<Option<Result<String, RpcError>>>,
}
fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
impl MicrosoftLogin {
    pub fn begin(
        &self,
        id: &str,
        address: &str,
        authentication: mail_core::Authentication,
    ) -> Result<Reply, RpcError> {
        let mut current = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = current.as_ref() {
            if entry.expires > Instant::now() && !entry.cancel.load(Ordering::Acquire) {
                if entry.id == id {
                    return Ok(Reply::MicrosoftLoginStarted {
                        session: entry.id.clone(),
                        authorization_url: entry.url.clone(),
                    });
                }
                return Err(error(ErrorCode::Busy, "已有微软登录，请先完成或取消"));
            }
            Self::cancel_entry(entry);
        }
        {
            let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            workers.retain(|worker| !worker.is_finished());
            if workers.len() >= 4 {
                return Err(error(ErrorCode::Busy, "请等待上次登录取消完成后重试"));
            }
        }
        let login = Login::begin(mail_service::microsoft::CLIENT_ID, address, authentication)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        let entry = Arc::new(Entry {
            id: id.into(),
            url: login.authorization_url.clone(),
            cancel: AtomicBool::new(false),
            expires: Instant::now() + Duration::from_secs(300),
            result: Mutex::new(None),
        });
        let worker_entry = entry.clone();
        let worker = thread::Builder::new()
            .name("qingmail-microsoft-login".into())
            .spawn(move || {
                let result = login
                    .finish(&worker_entry.cancel)
                    .and_then(|tokens| {
                        Credentials::microsoft(tokens)
                            .map_err(|_| mail_service::microsoft::Error::Vault)
                    })
                    .and_then(|credentials| {
                        SystemVault
                            .stage(&credentials)
                            .map_err(|_| mail_service::microsoft::Error::Vault)
                    })
                    .map_err(|e| error(ErrorCode::Authentication, e.to_string()));
                // The lock serializes cancellation with publication and cleanup of the secret reference.
                let mut state = worker_entry
                    .result
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if worker_entry.cancel.load(Ordering::Acquire) {
                    if let Ok(reference) = result {
                        let _ = SystemVault.discard_staged(&reference);
                    }
                } else {
                    *state = Some(result);
                }
            })
            .map_err(|_| error(ErrorCode::Busy, "无法启动微软登录"))?;
        let reply = Reply::MicrosoftLoginStarted {
            session: entry.id.clone(),
            authorization_url: entry.url.clone(),
        };
        *current = Some(entry);
        let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        workers.retain(|w| !w.is_finished());
        workers.push(worker);
        Ok(reply)
    }
    pub fn poll(&self, id: &str) -> Result<Reply, RpcError> {
        let current = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = current
            .as_ref()
            .filter(|e| e.id == id)
            .ok_or_else(|| error(ErrorCode::NotFound, "登录已结束，请重新登录"))?;
        if entry.expires <= Instant::now() || entry.cancel.load(Ordering::Acquire) {
            Self::cancel_entry(entry);
            return Err(error(
                ErrorCode::Authentication,
                "登录已取消或超时，请重新登录",
            ));
        }
        match &*entry.result.lock().unwrap_or_else(|e| e.into_inner()) {
            None => Ok(Reply::MicrosoftLoginPending),
            Some(Ok(reference)) => Ok(Reply::MicrosoftLoginReady {
                credential_ref: reference.clone(),
            }),
            Some(Err(error)) => Err(error.clone()),
        }
    }
    fn cancel_entry(entry: &Entry) {
        entry.cancel.store(true, Ordering::Release);
        if let Some(Ok(reference)) = entry
            .result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = SystemVault.discard_staged(&reference);
        }
    }
    pub fn cancel(&self, id: &str) -> Result<Reply, RpcError> {
        let current = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = current.as_ref().filter(|entry| entry.id == id) {
            Self::cancel_entry(entry);
        }
        Ok(Reply::Acknowledged)
    }
    pub fn stop(&self) {
        if let Some(entry) = self.entry.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Self::cancel_entry(&entry);
        }
        for worker in self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = worker.join();
        }
    }
}
impl Drop for MicrosoftLogin {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn microsoft_login_is_idempotent_bounded_cancelable_and_secret_free() {
        let login = MicrosoftLogin::default();
        let id = "a".repeat(32);
        let begin = || {
            login.begin(
                &id,
                "test@outlook.com",
                mail_core::Authentication::MicrosoftPersonal,
            )
        };
        let Reply::MicrosoftLoginStarted {
            session,
            authorization_url,
        } = begin().unwrap()
        else {
            panic!()
        };
        assert_eq!(session, id);
        assert!(authorization_url.contains(mail_service::microsoft::CLIENT_ID));
        let Reply::MicrosoftLoginStarted {
            authorization_url: same,
            ..
        } = begin().unwrap()
        else {
            panic!()
        };
        assert_eq!(authorization_url, same);
        assert!(matches!(login.poll(&id), Ok(Reply::MicrosoftLoginPending)));
        assert_eq!(
            login
                .begin(
                    &"b".repeat(32),
                    "test@outlook.com",
                    mail_core::Authentication::MicrosoftPersonal
                )
                .unwrap_err()
                .code,
            ErrorCode::Busy
        );
        assert!(matches!(login.cancel(&id), Ok(Reply::Acknowledged)));
        assert!(login.poll(&id).is_err());
        assert!(login.poll(&"unknown".repeat(5)).is_err());
        login.stop();
    }
}
