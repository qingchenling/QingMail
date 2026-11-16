//! P0–P2 进程内后台入口。激活通道仅为 P0 私有控制，不冻结 P3 IPC。
mod instance;
pub mod sending;
pub mod storage;
mod tray;

pub use instance::{Command, Instance, Launch, control};
pub use tray::Tray;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// 合成工作计数器：无账号、无网络、无发送；独立线程证明关窗不会停止后台。
pub struct Background {
    ticks: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    started: Instant,
}
impl Background {
    pub fn start() -> std::io::Result<Self> {
        let ticks = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_ticks, worker_stop) = (ticks.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("qingmail-p0-worker".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    thread::park_timeout(Duration::from_millis(250));
                    if !worker_stop.load(Ordering::Acquire) {
                        worker_ticks.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok(Self {
            ticks,
            stop,
            thread: Some(thread),
            started: Instant::now(),
        })
    }
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 私有激活监听独立于 UI。Drop 等待有界读取结束后释放实例锁。
pub struct ControlServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ControlServer {
    pub fn start(
        instance: Instance,
        queue: std::sync::mpsc::SyncSender<Command>,
        status: Arc<std::sync::Mutex<String>>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::Builder::new()
            .name("qingmail-p0-activation".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let status = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Err(e) = instance.poll(&status, &queue) {
                        eprintln!("P0 激活请求未处理：{e}");
                    }
                    thread::park_timeout(Duration::from_millis(20));
                }
                drop(instance);
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_stops_and_joins_explicitly() {
        let mut worker = Background::start().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(worker.ticks() > 0);
        worker.stop();
        let ticks = worker.ticks();
        assert!(worker.thread.is_none());
        assert_eq!(ticks, worker.ticks());
    }
}
