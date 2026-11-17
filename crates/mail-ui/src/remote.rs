//! UI 会话与后台状态轮询，所有 socket 等待位于独立线程。
use mail_protocol::{transport::Client, *};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
#[derive(Clone)]
pub struct View {
    pub snapshot: AppSnapshot,
    pub error: Option<String>,
    pub activation: Option<Activation>,
    pub shutdown: bool,
    pub completion: Option<Result<(), String>>,
}
enum Control {
    Shutdown,
    Finish(bool),
    Detach,
    Pause(bool),
    Stop,
}
pub struct Remote {
    cancel_shutdown: Arc<AtomicBool>,
    pub client: Client,
    pub saving: Arc<AtomicBool>,
    pub ack: Arc<AtomicU64>,
    pub view: Arc<Mutex<View>>,
    queue: mpsc::Sender<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Remote {
    pub fn start(front: mail_daemon::launcher::Frontend) -> Result<Self, String> {
        let (queue, receiver) = mpsc::channel();
        let client = front.client.clone();
        let saving = Arc::new(AtomicBool::new(front.snapshot.status.account_saving));
        let active = saving.clone();
        let ack = Arc::new(AtomicU64::new(0));
        let acknowledged = ack.clone();
        let view = Arc::new(Mutex::new(View {
            snapshot: front.snapshot.clone(),
            error: None,
            activation: None,
            shutdown: false,
            completion: None,
        }));
        let shared = view.clone();
        let cancel_shutdown = Arc::new(AtomicBool::new(false));
        let cancelled = cancel_shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("qingmail-ui-session".into())
            .spawn(move || {
                let mut shutdown_intent = false;
                let mut epoch = front.snapshot.epoch;
                loop {
                    let control = receiver.recv_timeout(std::time::Duration::from_millis(500));
                    let mut finish = None;
                    match control {
                        Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Ok(Control::Shutdown) => shutdown_intent = true,
                        Ok(Control::Pause(paused)) => {
                            let _ = front.client.request(Command::PauseSync {
                                account: None,
                                paused,
                            });
                        }
                        Ok(Control::Finish(saved)) => {
                            shutdown_intent = false;
                            finish = Some(Command::FinishShutdown {
                                session: front.session.clone(),
                                saved,
                            });
                        }
                        Ok(Control::Detach) => {
                            finish = Some(Command::DetachUi {
                                session: front.session.clone(),
                            })
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                    }
                    if let Some(command) = finish {
                        let saved = matches!(command, Command::FinishShutdown { saved: false, .. });
                        let result = front
                            .client
                            .request(command)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        if !saved {
                            shared.lock().unwrap_or_else(|e| e.into_inner()).completion =
                                Some(result);
                            continue;
                        }
                    }
                    if shutdown_intent {
                        let _ = front.client.request(Command::PrepareShutdown);
                    }
                    let result = (|| {
                        let reply = front.client.request(Command::UiHeartbeat {
                            session: front.session.clone(),
                            acknowledged: acknowledged.load(Ordering::Acquire),
                        });
                        let heartbeat = match reply {
                            Ok(reply) => reply,
                            Err(_) => {
                                let Reply::Hello {
                                    epoch: new_epoch, ..
                                } = front
                                    .client
                                    .request(Command::Hello)
                                    .map_err(|e| e.to_string())?
                                else {
                                    return Err("后台协议不兼容".into());
                                };
                                let Reply::UiAttached { attached: true } = front
                                    .client
                                    .request(Command::AttachUi {
                                        session: front.session.clone(),
                                    })
                                    .map_err(|e| e.to_string())?
                                else {
                                    return Err("已有另一前台，当前会话未恢复；请先保存草稿".into());
                                };
                                if epoch != new_epoch {
                                    acknowledged.store(0, Ordering::Release);
                                    epoch = new_epoch;
                                }
                                front
                                    .client
                                    .request(Command::UiHeartbeat {
                                        session: front.session.clone(),
                                        acknowledged: acknowledged.load(Ordering::Acquire),
                                    })
                                    .map_err(|e| e.to_string())?
                            }
                        };
                        let Reply::UiState {
                            activation,
                            shutdown_requested,
                        } = heartbeat
                        else {
                            return Err("会话响应无效".into());
                        };
                        let Reply::Snapshot(snapshot) = front
                            .client
                            .request(Command::Snapshot)
                            .map_err(|e| e.to_string())?
                        else {
                            return Err("后台快照响应无效".into());
                        };
                        active.store(snapshot.status.account_saving, Ordering::Release);
                        let mut view = shared.lock().unwrap_or_else(|e| e.into_inner());
                        view.snapshot = snapshot;
                        view.activation = activation;
                        if !shutdown_requested {
                            cancelled.store(false, Ordering::Release);
                        }
                        view.shutdown = shutdown_requested && !cancelled.load(Ordering::Acquire);
                        view.error = None;
                        Ok::<_, String>(())
                    })();
                    if let Err(error) = result {
                        shared.lock().unwrap_or_else(|e| e.into_inner()).error = Some(format!(
                            "后台连接不可用：{error}；已保存的数据保留，未确认操作不会重放"
                        ));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            cancel_shutdown,
            client,
            saving,
            ack,
            view,
            queue,
            thread: Some(thread),
        })
    }
    pub fn snapshot(&self) -> View {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn shutdown(&self) {
        let _ = self.queue.send(Control::Shutdown);
    }
    pub fn finish(&self, saved: bool) {
        if !saved {
            self.cancel_shutdown.store(true, Ordering::Release);
            self.view.lock().unwrap_or_else(|e| e.into_inner()).shutdown = false;
        }
        let _ = self.queue.send(Control::Finish(saved));
    }
    pub fn detach(&self) {
        let _ = self.queue.send(Control::Detach);
    }
    pub fn pause(&self, paused: bool) {
        let _ = self.queue.send(Control::Pause(paused));
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.queue.send(Control::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
