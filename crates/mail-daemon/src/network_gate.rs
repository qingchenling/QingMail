//! 主动 IMAP 任务上限与账号串行化；前台请求优先，后台同优先级按到达顺序。
use std::{
    collections::{HashSet, VecDeque},
    sync::{Condvar, Mutex, OnceLock},
};
#[derive(Default)]
struct State {
    active: HashSet<String>,
    queue: VecDeque<(u64, String, bool)>,
    sequence: u64,
}
#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<State>,
    wake: Condvar,
}
pub(crate) struct Permit<'a> {
    gate: &'a Gate,
    account: String,
}
impl Gate {
    pub fn acquire(&self, account: &str, foreground: bool) -> Permit<'_> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.sequence += 1;
        let ticket = state.sequence;
        state.queue.push_back((ticket, account.into(), foreground));
        loop {
            let eligible = state
                .queue
                .iter()
                .filter(|(_, a, _)| !state.active.contains(a))
                .max_by_key(|(id, _, priority)| (*priority, std::cmp::Reverse(*id)))
                .map(|(id, _, _)| *id);
            if state.active.len() < 2 && eligible == Some(ticket) {
                state.queue.retain(|(id, _, _)| *id != ticket);
                state.active.insert(account.into());
                return Permit {
                    gate: self,
                    account: account.into(),
                };
            }
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .remove(&self.account);
        self.gate.wake.notify_all();
    }
}
pub(crate) fn acquire(account: &mail_core::AccountId, foreground: bool) -> Permit<'static> {
    static GATE: OnceLock<Gate> = OnceLock::new();
    GATE.get_or_init(Gate::default)
        .acquire(&account.0, foreground)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };
    fn queued(gate: &Gate, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if gate.state.lock().unwrap().queue.len() == count {
                return;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
    #[test]
    fn reading_precedes_background_without_overtaking_active_account() {
        let gate = Arc::new(Gate::default());
        let a = gate.acquire("a", false);
        let b = gate.acquire("b", false);
        let (tx, rx) = mpsc::channel();
        let g = gate.clone();
        let t = tx.clone();
        let background = thread::spawn(move || {
            let _p = g.acquire("c", false);
            t.send("background").unwrap();
        });
        queued(&gate, 1);
        let g = gate.clone();
        let foreground = thread::spawn(move || {
            let _p = g.acquire("a", true);
            tx.send("reading").unwrap();
        });
        queued(&gate, 2);
        drop(a);
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "reading");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            "background"
        );
        drop(b);
        background.join().unwrap();
        foreground.join().unwrap();
    }
    #[test]
    fn five_accounts_progress_after_failure_with_two_active_and_same_account_serial() {
        let gate = Arc::new(Gate::default());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        thread::scope(|scope| {
            for id in 0..10 {
                let (gate, active, maximum, completed) = (
                    gate.clone(),
                    active.clone(),
                    maximum.clone(),
                    completed.clone(),
                );
                scope.spawn(move || {
                    let _p = gate.acquire(&format!("account-{}", id % 5), false);
                    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(n, Ordering::SeqCst);
                    assert!(n <= 2);
                    thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                    if id != 0 {
                        completed.fetch_add(1, Ordering::SeqCst);
                    } // 错误路径同样释放许可
                });
            }
        });
        assert_eq!(completed.load(Ordering::SeqCst), 9);
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }
}
