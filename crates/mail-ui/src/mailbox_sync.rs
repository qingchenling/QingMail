//! 底栏同步状态快照；失败保留上次结果，不能把未知状态显示为全部完成。
use mail_daemon::storage::{StorageClient, receive};
use mail_protocol::{Command, DraftSyncInfo, OperationInfo, OperationState, Reply};
use mail_storage::SendTask;

pub(super) struct AccountDraft {
    pub address: String,
    pub info: DraftSyncInfo,
}

#[derive(Default)]
pub(super) struct Snapshot {
    pub operations: Vec<OperationInfo>,
    pub tasks: Vec<SendTask>,
    pub drafts: Vec<AccountDraft>,
}

#[derive(Default)]
pub(super) struct Status {
    pub data: Snapshot,
    pub loaded: bool,
    pub error: Option<String>,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Summary {
    pub operations: usize,
    pub tasks: usize,
    pub drafts: usize,
    pub issues: usize,
}

impl Summary {
    pub fn pending(&self) -> usize {
        self.operations + self.tasks + self.drafts
    }
}

pub(super) fn draft_pending(info: &DraftSyncInfo) -> bool {
    info.state != "stopped" && (info.state != "synced" || info.revision != info.base_revision)
}

impl Status {
    pub fn update(&mut self, result: Result<Snapshot, String>) {
        match result {
            Ok(data) => {
                self.data = data;
                self.loaded = true;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
    pub fn summary(&self) -> Summary {
        use mail_core::SendState;
        let operations = self
            .data
            .operations
            .iter()
            .filter(|o| o.state != OperationState::Applied)
            .count();
        let (tasks, _) = super::sync_counts(&self.data.tasks);
        let drafts = self
            .data
            .drafts
            .iter()
            .filter(|d| draft_pending(&d.info))
            .count();
        let issues = self
            .data
            .operations
            .iter()
            .filter(|o| {
                matches!(
                    o.state,
                    OperationState::Failed | OperationState::Conflict | OperationState::Uncertain
                )
            })
            .count()
            + self
                .data
                .tasks
                .iter()
                .filter(|t| matches!(t.state, SendState::Failed | SendState::Uncertain))
                .count()
            + self
                .data
                .drafts
                .iter()
                .filter(|d| matches!(d.info.state.as_str(), "failed" | "conflict" | "uncertain"))
                .count();
        Summary {
            operations,
            tasks,
            drafts,
            issues,
        }
    }
    pub fn indicator(&self) -> (&'static str, String) {
        let summary = self.summary();
        if self.error.is_some() {
            ("sync_problem", "无法读取最新同步状态，点击查看".into())
        } else if !self.loaded {
            ("sync", "正在读取同步状态".into())
        } else if summary.issues > 0 {
            (
                "sync_problem",
                format!(
                    "有 {} 项未完成，其中 {} 项需要处理",
                    summary.pending(),
                    summary.issues
                ),
            )
        } else if summary.pending() > 0 {
            ("sync", format!("有 {} 项尚未同步完成", summary.pending()))
        } else {
            ("check_circle", "本地变更已同步".into())
        }
    }
}

pub(super) fn read(store: &StorageClient) -> Result<Snapshot, String> {
    let operations = match store.control(Command::Operations { account: None })? {
        Reply::Operations(items) => items,
        _ => return Err("同步状态响应无效".into()),
    };
    let mut data = Snapshot {
        operations,
        ..Default::default()
    };
    for (account, settings) in receive(store.accounts())? {
        data.tasks.extend(receive(store.tasks(account.clone()))?);
        let drafts = match store.control(Command::DraftSyncStatus {
            account: account.clone(),
        })? {
            Reply::DraftSyncStatus(items) => items,
            _ => return Err("草稿同步状态响应无效".into()),
        };
        data.drafts.extend(
            drafts
                .into_iter()
                .filter(draft_pending)
                .map(|info| AccountDraft {
                    address: settings.address.clone(),
                    info,
                }),
        );
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft(state: &str, revision: u32, base_revision: u32) -> AccountDraft {
        AccountDraft {
            address: "fixture@example.invalid".into(),
            info: DraftSyncInfo {
                updated_at: 0,
                draft: mail_core::DraftId("b".repeat(32)),
                subject: "draft".into(),
                revision,
                base_revision,
                state: state.into(),
                detail: String::new(),
                source: None,
            },
        }
    }
    #[test]
    fn drafts_drive_pending_conflict_and_verified_completion() {
        let mut status = Status::default();
        assert_eq!(status.indicator().0, "sync");
        for state in ["pending", "failed", "conflict", "uncertain"] {
            status.update(Ok(Snapshot {
                drafts: vec![draft(state, 2, 1)],
                ..Default::default()
            }));
            assert_eq!(status.summary().drafts, 1);
            assert_eq!(
                status.indicator().0,
                if state == "pending" {
                    "sync"
                } else {
                    "sync_problem"
                }
            );
        }
        status.update(Ok(Snapshot {
            drafts: vec![draft("synced", 2, 1)],
            ..Default::default()
        }));
        assert_eq!(
            status.summary().pending(),
            1,
            "an old server baseline cannot clear a newer local edit"
        );
        status.update(Ok(Snapshot {
            drafts: vec![draft("synced", 2, 2)],
            ..Default::default()
        }));
        assert_eq!(status.summary().pending(), 0);
        assert_eq!(status.indicator().0, "check_circle");
        status.update(Ok(Snapshot {
            drafts: vec![draft("stopped", 2, 1)],
            ..Default::default()
        }));
        assert_eq!(status.summary().pending(), 0);
        status.update(Ok(Snapshot {
            drafts: vec![draft("deleting", 2, 2)],
            ..Default::default()
        }));
        assert_eq!(status.summary().drafts, 1);
        assert_eq!(status.indicator().0, "sync");
    }
    #[test]
    fn read_failure_preserves_last_known_items_and_never_reports_completion() {
        let mut status = Status::default();
        status.update(Err("daemon disconnected".into()));
        assert!(!status.loaded);
        assert_eq!(status.indicator().0, "sync_problem");
        status.update(Ok(Snapshot {
            drafts: vec![draft("pending", 1, 0)],
            ..Default::default()
        }));
        status.update(Err("daemon restarted".into()));
        assert_eq!(status.summary().pending(), 1);
        assert_eq!(status.indicator().0, "sync_problem");
        status.update(Ok(Snapshot::default()));
        assert!(status.error.is_none());
        assert_eq!(status.indicator().0, "check_circle");
    }
}
