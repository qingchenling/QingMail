//! 单前台租约与两阶段退出。租约过期不授权重放任何业务写入。
use super::rpc_error;
use mail_protocol::*;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const LEASE: Duration = Duration::from_secs(15);
const EVENTS: usize = 256;
struct Frontend {
    token: String,
    seen: Instant,
    activation: Option<Activation>,
}
#[derive(Default)]
pub(super) struct Sessions {
    frontend: Option<Frontend>,
    sequence: u64,
    shutdown: bool,
    revision: u64,
    observed: (u64, u64),
    events: VecDeque<Event>,
}
impl Sessions {
    fn expire(&mut self, now: Instant) {
        if self
            .frontend
            .as_ref()
            .is_some_and(|ui| now.saturating_duration_since(ui.seen) >= LEASE)
        {
            self.frontend = None;
            // 前台崩溃不相当于“已保存”；必须由用户再次请求退出。
            self.shutdown = false;
        }
    }
    pub fn observe(&mut self, business: u64, sync: u64) {
        if business != self.observed.0 {
            self.emit(EventTopic::Accounts);
            self.emit(EventTopic::Drafts);
            self.emit(EventTopic::Sending);
            self.emit(EventTopic::Resources);
        }
        if sync != self.observed.1 {
            self.emit(EventTopic::Mail);
        }
        self.observed = (business, sync);
    }
    fn emit(&mut self, topic: EventTopic) {
        self.revision += 1;
        self.events.push_back(Event {
            revision: self.revision,
            topic,
            account: None,
        });
        if self.events.len() > EVENTS {
            self.events.pop_front();
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn events(&self, after: u64) -> EventBatch {
        let resnapshot = after > self.revision
            || self
                .events
                .front()
                .is_some_and(|event| after < event.revision.saturating_sub(1));
        EventBatch {
            revision: self.revision,
            resnapshot,
            events: if resnapshot {
                vec![]
            } else {
                self.events
                    .iter()
                    .filter(|event| event.revision > after)
                    .cloned()
                    .collect()
            },
        }
    }
    pub fn attach(&mut self, token: &str, now: Instant) -> Result<Reply, RpcError> {
        self.expire(now);
        if self.shutdown {
            return Err(rpc_error(ErrorCode::Busy, "后台正在协调退出"));
        }
        let attached = match &mut self.frontend {
            Some(ui) if ui.token == token => {
                ui.seen = now;
                true
            }
            Some(_) => {
                self.activate(WindowTarget::Mailbox, now)?;
                false
            }
            None => {
                self.frontend = Some(Frontend {
                    token: token.into(),
                    seen: now,
                    activation: None,
                });
                true
            }
        };
        Ok(Reply::UiAttached { attached })
    }
    pub fn activate(&mut self, target: WindowTarget, now: Instant) -> Result<Reply, RpcError> {
        self.expire(now);
        let ui = self
            .frontend
            .as_mut()
            .ok_or_else(|| rpc_error(ErrorCode::NotFound, "没有活动前台"))?;
        self.sequence += 1;
        ui.activation = Some(Activation {
            sequence: self.sequence,
            target,
        });
        Ok(Reply::Acknowledged)
    }
    fn check(&mut self, token: &str, now: Instant) -> Result<&mut Frontend, RpcError> {
        self.expire(now);
        self.frontend
            .as_mut()
            .filter(|ui| ui.token == token)
            .ok_or_else(|| rpc_error(ErrorCode::Conflict, "前台会话已失效，请重新连接"))
    }
    pub fn heartbeat(
        &mut self,
        token: &str,
        acknowledged: u64,
        now: Instant,
    ) -> Result<Reply, RpcError> {
        let ui = self.check(token, now)?;
        ui.seen = now;
        // 超前确认不能吞掉后续激活，通常意味着客户端持有旧 epoch 的计数。
        if ui
            .activation
            .as_ref()
            .is_some_and(|a| acknowledged > a.sequence)
        {
            return Err(rpc_error(ErrorCode::InvalidRequest, "激活确认超出当前序号"));
        }
        if ui
            .activation
            .as_ref()
            .is_some_and(|a| acknowledged == a.sequence)
        {
            ui.activation = None;
        }
        let activation = ui.activation.clone();
        Ok(Reply::UiState {
            activation,
            shutdown_requested: self.shutdown,
        })
    }
    pub fn detach(&mut self, token: &str, now: Instant) -> Result<Reply, RpcError> {
        self.check(token, now)?;
        if self.shutdown {
            return Err(rpc_error(ErrorCode::Conflict, "退出协调中，请报告保存结果"));
        }
        self.frontend = None;
        Ok(Reply::Acknowledged)
    }
    /// true 仅表示不存在需要保存的前台；调用方仍须检查后台正在执行的业务。
    pub fn prepare(&mut self, now: Instant) -> bool {
        self.expire(now);
        if self.frontend.is_none() {
            return true;
        }
        if !self.shutdown {
            self.shutdown = true;
            self.emit(EventTopic::Shutdown);
        }
        false
    }
    pub fn finish(&mut self, token: &str, saved: bool, now: Instant) -> Result<bool, RpcError> {
        self.check(token, now)?;
        if !self.shutdown {
            return Err(rpc_error(ErrorCode::Conflict, "没有待完成的退出请求"));
        }
        self.shutdown = false;
        if saved {
            self.frontend = None;
        }
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_activation_ack_and_crash_recovery() {
        let mut s = Sessions::default();
        let now = Instant::now();
        assert!(matches!(
            s.attach("first", now).unwrap(),
            Reply::UiAttached { attached: true }
        ));
        assert!(matches!(
            s.attach("second", now).unwrap(),
            Reply::UiAttached { attached: false }
        ));
        let Reply::UiState {
            activation: Some(a),
            ..
        } = s.heartbeat("first", 0, now).unwrap()
        else {
            panic!()
        };
        assert!(s.heartbeat("second", a.sequence, now).is_err());
        assert!(matches!(
            s.heartbeat("first", a.sequence, now).unwrap(),
            Reply::UiState {
                activation: None,
                ..
            }
        ));
        assert!(!s.prepare(now));
        assert!(matches!(
            s.attach("second", now + LEASE).unwrap(),
            Reply::UiAttached { attached: true }
        ));
        assert!(s.finish("first", true, now + LEASE).is_err());
    }
    #[test]
    fn shutdown_needs_save_and_can_be_cancelled() {
        let mut s = Sessions::default();
        let now = Instant::now();
        s.attach("first", now).unwrap();
        assert!(!s.prepare(now));
        assert!(matches!(
            s.heartbeat("first", 0, now).unwrap(),
            Reply::UiState {
                shutdown_requested: true,
                ..
            }
        ));
        assert!(s.detach("first", now).is_err());
        assert!(!s.finish("first", false, now).unwrap());
        assert!(!s.prepare(now));
        assert!(s.finish("first", true, now).unwrap());
        assert!(s.prepare(now));
    }
    #[test]
    fn missing_events_require_snapshot_and_observation_is_coalesced() {
        let mut s = Sessions::default();
        s.observe(1, 1);
        let r = s.revision();
        s.observe(1, 1);
        assert_eq!(r, s.revision());
        for _ in 0..EVENTS {
            s.emit(EventTopic::Mail);
        }
        assert!(s.events(0).resnapshot);
        assert!(!s.events(s.revision() - 1).resnapshot);
        assert!(s.events(s.revision() + 1).resnapshot);
    }
}
