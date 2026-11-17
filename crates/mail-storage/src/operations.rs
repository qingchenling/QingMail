//! 操作和本地意图同事务落盘；服务器索引独立保存，投影视图叠加用户意图。
use crate::*;
use mail_protocol::{
    Command, ErrorCode, MailOperation, MutationStatus, OperationInfo, OperationResolution,
    OperationState, Reply, Request, RpcError,
};
fn state(s: OperationState) -> &'static str {
    match s {
        OperationState::Pending => "pending",
        OperationState::InFlight => "in_flight",
        OperationState::Applied => "applied",
        OperationState::Failed => "failed",
        OperationState::Conflict => "conflict",
        OperationState::Uncertain => "uncertain",
    }
}
pub(super) fn recover(db: &Connection) -> Result<()> {
    db.execute("UPDATE mail_operations SET state=CASE WHEN json_extract(operation,'$.type')='flags' THEN 'pending' ELSE 'uncertain' END,detail=?1 WHERE state='in_flight'", [serde_json::to_string(&RpcError{code:ErrorCode::Uncertain,message:"后台中断；标记可安全恢复，移动须先核对远端".into()}).unwrap()])?;
    Ok(())
}
impl Store {
    /// 请求结果、每封操作与投影所需数据原子提交，丢响应后同ID只返回原结果。
    pub fn enqueue_operations(&mut self, request: &Request) -> Result<Reply> {
        self.enqueue_operations_with_trash(request, &std::collections::BTreeMap::new())
    }
    pub fn enqueue_operations_with_trash(
        &mut self,
        request: &Request,
        trash: &std::collections::BTreeMap<String, FolderId>,
    ) -> Result<Reply> {
        let Command::EnqueueOperation {
            locations,
            operation,
        } = &request.command
        else {
            return Err(Error::Invalid("操作请求无效"));
        };
        request
            .validate()
            .map_err(|_| Error::Invalid("操作请求无效"))?;
        let fingerprint = mutations::fingerprint(request)?;
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT fingerprint FROM mutation_results WHERE request_id=?1",
                [&request.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            if previous != fingerprint {
                return Err(Error::Conflict);
            }
            return match self.mutation_result(&request.id)? {
                MutationStatus::Finished(r) => r.map_err(|_| Error::Conflict),
                _ => Err(Error::Conflict),
            };
        }
        if locations.is_empty()
            || locations.len() > 200
            || matches!(
                operation,
                MailOperation::Flags {
                    seen: None,
                    flagged: None
                }
            )
        {
            return Err(Error::Invalid("操作为空或超过200封"));
        }
        let tx = self.connection.transaction()?;
        let pending: u32 = tx.query_row(
            "SELECT COUNT(*) FROM mail_operations WHERE state!='applied'",
            [],
            |r| r.get(0),
        )?;
        if pending as usize + locations.len() > 200 {
            return Err(Error::Invalid("待处理操作已达200封上限，请先处理现有任务"));
        }
        let mut records = vec![];
        let mut unique = std::collections::HashSet::new();
        for location in locations {
            if !unique.insert(location) {
                continue;
            }
            let source:Option<String>=tx.query_row("SELECT f.remote_name FROM folders f JOIN folder_messages m ON m.account_id=f.account_id AND m.folder_id=f.id AND m.uidvalidity=f.uidvalidity WHERE f.account_id=?1 AND f.id=?2 AND f.uidvalidity=?3 AND m.uid=?4",params![location.account.0,location.folder_id,location.uidvalidity,location.uid],|r|r.get(0)).optional()?;
            let source = source.ok_or(Error::Invalid("邮件定位已变化，请刷新"))?;
            let op = match operation {
                MailOperation::Delete => MailOperation::Move {
                    destination: trash
                        .get(&location.account.0)
                        .ok_or(Error::Invalid("未找到已同步的垃圾箱"))?
                        .clone(),
                },
                op => op.clone(),
            };
            if let MailOperation::Move { destination } = &op {
                let target:Option<String>=tx.query_row("SELECT f.remote_name FROM folders f JOIN remote_folders r ON r.account_id=f.account_id AND r.remote_name=f.remote_name WHERE f.account_id=?1 AND f.id=?2 AND r.selectable=1",params![location.account.0,destination.0],|r|r.get(0)).optional()?;
                if target.is_none() || target.as_ref() == Some(&source) {
                    return Err(Error::Invalid("目标文件夹无效或跨账号"));
                }
            }
            let id = new_id()?;
            let encoded = serde_json::to_string(&op).map_err(|_| Error::Invalid("操作无法编码"))?;
            tx.execute("INSERT INTO mail_operations(id,account_id,folder_id,uidvalidity,uid,operation,state) VALUES (?1,?2,?3,?4,?5,?6,'pending')",params![id,location.account.0,location.folder_id,location.uidvalidity,location.uid,encoded])?;
            records.push(OperationInfo {
                id,
                location: location.clone(),
                operation: op,
                state: OperationState::Pending,
                detail: None,
            });
        }
        let reply = Reply::Operations(records);
        tx.execute(
            "INSERT INTO mutation_results VALUES (?1,?2,'in_progress',NULL,?3)",
            params![request.id, fingerprint, now()?],
        )?;
        mutations::finish_on(&tx, request, &Ok(reply.clone()))?;
        tx.commit()?;
        Ok(reply)
    }
    pub fn operations(&self, account: Option<&AccountId>) -> Result<Vec<OperationInfo>> {
        let mut q=self.connection.prepare("SELECT id,account_id,folder_id,uidvalidity,uid,operation,state,detail FROM mail_operations WHERE (?1 IS NULL OR account_id=?1) ORDER BY (state='applied'),seq DESC LIMIT 200")?;
        let rows = q
            .query_map([account.map(|a| &a.0)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    MailUid {
                        account: AccountId(r.get(1)?),
                        folder_id: r.get(2)?,
                        uidvalidity: r.get(3)?,
                        uid: r.get(4)?,
                    },
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, location, op, s, detail)| {
                Ok(OperationInfo {
                    id,
                    location,
                    operation: serde_json::from_str(&op)
                        .map_err(|_| Error::Invalid("操作记录损坏"))?,
                    state: serde_json::from_str(&format!("\"{s}\""))
                        .map_err(|_| Error::Invalid("操作状态损坏"))?,
                    detail: detail
                        .map(|d| serde_json::from_str(&d))
                        .transpose()
                        .map_err(|_| Error::Invalid("操作原因损坏"))?,
                })
            })
            .collect()
    }
    /// 每账号保序；失败/冲突/不确定会阻止后续操作越过。其他账号仍可推进。
    pub fn claim_operation(&mut self) -> Result<Option<(OperationInfo, bool)>> {
        self.connection.execute("UPDATE mail_operations SET state='conflict',detail=?1 WHERE state='pending' AND NOT EXISTS(SELECT 1 FROM folders f WHERE f.account_id=mail_operations.account_id AND f.id=mail_operations.folder_id AND f.uidvalidity=mail_operations.uidvalidity)",[serde_json::to_string(&RpcError{code:ErrorCode::Conflict,message:"UIDVALIDITY或文件夹已变化；旧UID操作已停止".into()}).unwrap()])?;
        let id:Option<String>=self.connection.query_row("SELECT o.id FROM mail_operations o JOIN account_options a ON a.account_id=o.account_id WHERE (o.state='pending' OR o.reconcile_requested=1) AND a.paused=0 AND o.retry_at<=?1 AND NOT EXISTS(SELECT 1 FROM mail_operations older WHERE older.account_id=o.account_id AND older.seq<o.seq AND older.state!='applied') ORDER BY o.retry_at,o.seq LIMIT 1",[now()?],|r|r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        let reconcile: bool = self.connection.query_row(
            "SELECT reconcile_requested FROM mail_operations WHERE id=?1",
            [&id],
            |r| r.get(0),
        )?;
        self.connection.execute("UPDATE mail_operations SET state='in_flight',reconcile_requested=0,attempts=attempts+1 WHERE id=?1",[&id])?;
        // Query by ID rather than the bounded user-facing history.
        self.operation_by_id(&id)
            .map(|info| Some((info, reconcile)))
    }
    fn operation_by_id(&self, id: &str) -> Result<OperationInfo> {
        let account: String = self.connection.query_row(
            "SELECT account_id FROM mail_operations WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        let mut q=self.connection.prepare("SELECT operation,folder_id,uidvalidity,uid,state,detail FROM mail_operations WHERE id=?1")?;
        let (op, folder_id, uidvalidity, uid, s, d): (
            String,
            String,
            u32,
            u32,
            String,
            Option<String>,
        ) = q.query_row([id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        Ok(OperationInfo {
            id: id.into(),
            location: MailUid {
                account: AccountId(account),
                folder_id,
                uidvalidity,
                uid,
            },
            operation: serde_json::from_str(&op).map_err(|_| Error::Invalid("操作记录损坏"))?,
            state: serde_json::from_str(&format!("\"{s}\""))
                .map_err(|_| Error::Invalid("操作状态损坏"))?,
            detail: d
                .map(|v| serde_json::from_str(&v))
                .transpose()
                .map_err(|_| Error::Invalid("操作原因损坏"))?,
        })
    }
    pub fn finish_operation(
        &mut self,
        id: &str,
        next: OperationState,
        detail: Option<RpcError>,
    ) -> Result<()> {
        let op = self.operation_by_id(id)?;
        if op.state != OperationState::InFlight {
            return Err(Error::Conflict);
        }
        let attempts: u32 = self.connection.query_row(
            "SELECT attempts FROM mail_operations WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        let next = if next == OperationState::Pending && attempts >= 3 {
            OperationState::Failed
        } else {
            next
        };
        let tx = self.connection.transaction()?;
        if next == OperationState::Applied {
            let l = &op.location;
            match op.operation {
                MailOperation::Flags { seen, flagged } => {
                    tx.execute("UPDATE folder_messages SET seen=COALESCE(?1,seen),flagged=COALESCE(?2,flagged) WHERE account_id=?3 AND folder_id=?4 AND uidvalidity=?5 AND uid=?6",params![seen,flagged,l.account.0,l.folder_id,l.uidvalidity,l.uid])?;
                }
                _ => {
                    tx.execute("DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",params![l.account.0,l.folder_id,l.uidvalidity,l.uid])?;
                }
            }
        }
        tx.execute(
            "UPDATE mail_operations SET state=?1,detail=?2,retry_at=?3 WHERE id=?4",
            params![
                state(next),
                detail
                    .map(|d| serde_json::to_string(&d))
                    .transpose()
                    .map_err(|_| Error::Invalid("原因无法编码"))?,
                if next == OperationState::Pending {
                    now()? + 60
                } else {
                    0
                },
                id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn retry_operation(&mut self, id: &str) -> Result<()> {
        let op = self.operation_by_id(id)?;
        if !matches!(op.state, OperationState::Failed) {
            return Err(Error::Invalid("仅明确失败可重试；冲突或不确定请先核对"));
        }
        self.connection.execute("UPDATE mail_operations SET state='pending',detail=NULL,retry_at=0,attempts=0 WHERE id=?1",[id])?;
        Ok(())
    }
    pub fn resolve_operation(&mut self, id: &str, resolution: OperationResolution) -> Result<()> {
        let op = self.operation_by_id(id)?;
        if !matches!(
            op.state,
            OperationState::Failed | OperationState::Conflict | OperationState::Uncertain
        ) {
            return Err(Error::Invalid("当前操作不能处理"));
        }
        match resolution {
            OperationResolution::KeepServer => {
                self.connection.execute(
                    "UPDATE mail_operations SET state='applied',detail=?1 WHERE id=?2",
                    params![
                        serde_json::to_string(&RpcError {
                            code: ErrorCode::Conflict,
                            message: "用户明确保留服务器状态并放弃本地意图".into()
                        })
                        .unwrap(),
                        id
                    ],
                )?;
            }
            OperationResolution::Reconcile => {
                self.connection.execute(
                    "UPDATE mail_operations SET reconcile_requested=1,retry_at=0 WHERE id=?1",
                    [id],
                )?;
            }
        }
        Ok(())
    }
}
fn now() -> Result<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| Error::Invalid("系统时间无效"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::settings;
    fn envelope() -> MessageEnvelope {
        MessageEnvelope {
            subject: "离线测试".into(),
            sender: "fixture@example.invalid".into(),
            received_at: 100,
        }
    }
    fn setup() -> (tempfile::TempDir, Store, MailUid, FolderId) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let a = store.create_account(&settings()).unwrap();
        store
            .replace_remote_folders(
                &a,
                &[
                    mail_core::RemoteFolder {
                        name: "INBOX".into(),
                        display_name: "收件箱".into(),
                        delimiter: Some("/".into()),
                        selectable: true,
                    },
                    mail_core::RemoteFolder {
                        name: "Trash".into(),
                        display_name: "垃圾箱".into(),
                        delimiter: Some("/".into()),
                        selectable: true,
                    },
                ],
                1,
            )
            .unwrap();
        let inbox = store.upsert_folder(&a, "INBOX", 42).unwrap();
        let trash = store.upsert_folder(&a, "Trash", 42).unwrap();
        let loc = MailUid {
            account: a,
            folder_id: inbox.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store.cache_message(&loc, &envelope()).unwrap();
        (root, store, loc, trash.id)
    }
    fn enqueue(store: &mut Store, loc: &MailUid, op: MailOperation) -> OperationInfo {
        let req = Request::new(
            new_id().unwrap(),
            Command::EnqueueOperation {
                locations: vec![loc.clone()],
                operation: op,
            },
        );
        let Reply::Operations(records) = store.enqueue_operations(&req).unwrap() else {
            panic!()
        };
        records[0].clone()
    }
    fn messages(store: &Store, loc: &MailUid) -> Vec<CachedMessage> {
        let f = store
            .folders(&loc.account)
            .unwrap()
            .into_iter()
            .find(|f| f.id.0 == loc.folder_id)
            .unwrap();
        store.list_messages(&f, None, 200).unwrap()
    }
    #[test]
    fn overlay_survives_server_refresh_restart_and_atomic_lost_response() {
        let (root, mut store, loc, _) = setup();
        let req = Request::new(
            new_id().unwrap(),
            Command::EnqueueOperation {
                locations: vec![loc.clone()],
                operation: MailOperation::Flags {
                    seen: Some(true),
                    flagged: Some(true),
                },
            },
        );
        store.enqueue_operations(&req).unwrap();
        store.enqueue_operations(&req).unwrap();
        assert_eq!(store.operations(None).unwrap().len(), 1);
        store
            .set_flags(&loc.account, "INBOX", 42, &[7], Some(false), Some(false))
            .unwrap();
        assert!(messages(&store, &loc)[0].seen && messages(&store, &loc)[0].flagged);
        assert!(store.unified_inbox(None, 10).unwrap().messages[0].seen);
        let query = SearchQuery {
            text: String::new(),
            sender: String::new(),
            folder: None,
            has_attachments: false,
        };
        assert!(store.search_scope(None, &query).unwrap()[0].message.seen);
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(messages(&store, &loc)[0].seen);
        assert!(matches!(
            store.mutation_result(&req.id).unwrap(),
            MutationStatus::Finished(_)
        ));
        let mut changed = req.clone();
        changed.command = Command::EnqueueOperation {
            locations: vec![loc.clone()],
            operation: MailOperation::Flags {
                seen: Some(false),
                flagged: None,
            },
        };
        assert!(matches!(
            store.enqueue_operations(&changed),
            Err(Error::Conflict)
        ));
        let mut invalid = loc.clone();
        invalid.uid = 99;
        let req = Request::new(
            new_id().unwrap(),
            Command::EnqueueOperation {
                locations: vec![loc.clone(), invalid],
                operation: MailOperation::Flags {
                    seen: Some(false),
                    flagged: None,
                },
            },
        );
        assert!(store.enqueue_operations(&req).is_err());
        assert_eq!(store.operations(None).unwrap().len(), 1);
        assert!(messages(&store, &loc)[0].seen);
        assert!(matches!(
            store.mutation_result(&req.id).unwrap(),
            MutationStatus::NotFound
        ));
    }
    #[test]
    fn interrupted_flags_retry_but_moves_remain_uncertain_and_block_only_their_account() {
        let (root, mut store, loc, trash) = setup();
        let flag = enqueue(
            &mut store,
            &loc,
            MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        );
        let (claimed, _) = store.claim_operation().unwrap().unwrap();
        assert_eq!(claimed.id, flag.id);
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(
            store.operation_by_id(&flag.id).unwrap().state,
            OperationState::Pending
        );
        store.claim_operation().unwrap().unwrap();
        store
            .finish_operation(&flag.id, OperationState::Applied, None)
            .unwrap();
        let moved = enqueue(&mut store, &loc, MailOperation::Move { destination: trash });
        assert!(messages(&store, &loc).is_empty());
        store.claim_operation().unwrap().unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(
            store.operation_by_id(&moved.id).unwrap().state,
            OperationState::Uncertain
        );
        assert!(store.retry_operation(&moved.id).is_err());
        assert!(store.claim_operation().unwrap().is_none());
        let other = store.create_account(&settings()).unwrap();
        let f = store.upsert_folder(&other, "INBOX", 42).unwrap();
        let other = MailUid {
            account: other,
            folder_id: f.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store.cache_message(&other, &envelope()).unwrap();
        let op = enqueue(
            &mut store,
            &other,
            MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        );
        assert_eq!(store.claim_operation().unwrap().unwrap().0.id, op.id);
        store
            .resolve_operation(&moved.id, OperationResolution::Reconcile)
            .unwrap();
        let (check, reconcile) = store.claim_operation().unwrap().unwrap();
        assert_eq!(check.id, moved.id);
        assert!(reconcile);
        store
            .finish_operation(&moved.id, OperationState::Uncertain, None)
            .unwrap();
        store
            .resolve_operation(&moved.id, OperationResolution::KeepServer)
            .unwrap();
        assert_eq!(messages(&store, &loc).len(), 1);
    }
    #[test]
    fn epoch_change_stops_old_uid_and_cross_account_destination_rolls_back() {
        let (_root, mut store, loc, trash) = setup();
        let a = store.create_account(&settings()).unwrap();
        let f = store.upsert_folder(&a, "INBOX", 42).unwrap();
        let mut wrong = loc.clone();
        wrong.account = a;
        wrong.folder_id = f.id.0;
        store.cache_message(&wrong, &envelope()).unwrap();
        let req = Request::new(
            new_id().unwrap(),
            Command::EnqueueOperation {
                locations: vec![wrong],
                operation: MailOperation::Move { destination: trash },
            },
        );
        assert!(store.enqueue_operations(&req).is_err());
        let op = enqueue(
            &mut store,
            &loc,
            MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        );
        store.upsert_folder(&loc.account, "INBOX", 43).unwrap();
        assert!(store.claim_operation().unwrap().is_none());
        assert_eq!(
            store.operation_by_id(&op.id).unwrap().state,
            OperationState::Conflict
        );
        assert!(store.retry_operation(&op.id).is_err());
    }
    #[test]
    fn queued_flags_remain_ordered_and_move_overlay_is_not_erased_by_server_sync() {
        let (_root, mut store, loc, trash) = setup();
        let first = enqueue(
            &mut store,
            &loc,
            MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        );
        let second = enqueue(
            &mut store,
            &loc,
            MailOperation::Flags {
                seen: Some(false),
                flagged: Some(true),
            },
        );
        assert!(!messages(&store, &loc)[0].seen);
        assert!(messages(&store, &loc)[0].flagged);
        store.claim_operation().unwrap();
        store
            .finish_operation(&first.id, OperationState::Applied, None)
            .unwrap();
        assert!(!messages(&store, &loc)[0].seen);
        assert_eq!(store.claim_operation().unwrap().unwrap().0.id, second.id);
        store
            .finish_operation(&second.id, OperationState::Applied, None)
            .unwrap();
        let moved = enqueue(&mut store, &loc, MailOperation::Move { destination: trash });
        store.cache_message(&loc, &envelope()).unwrap();
        assert!(messages(&store, &loc).is_empty());
        store.claim_operation().unwrap();
        store
            .finish_operation(&moved.id, OperationState::Applied, None)
            .unwrap();
        assert!(messages(&store, &loc).is_empty());
        assert!(
            store
                .operations(None)
                .unwrap()
                .iter()
                .all(|o| o.state == OperationState::Applied)
        );
    }
    #[test]
    fn paused_account_bounded_retry_and_schema11_upgrade() {
        let (root, store, loc, _) = setup();
        store.connection.execute_batch("DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; DROP TRIGGER stop_sent_draft_sync; DROP TRIGGER track_draft_scan_account; DROP TABLE draft_conflicts; DROP TABLE draft_scan_failures; DROP TABLE draft_scans; DROP TRIGGER track_server_draft_insert; DROP TRIGGER track_server_draft_update; DROP TABLE draft_uploads; DROP TABLE draft_sync; DROP TABLE server_draft_sources; DROP TABLE server_draft_folders; DROP TABLE part_resources; DROP TABLE message_structures; DROP VIEW visible_folder_messages; DROP TABLE mail_operations; PRAGMA user_version=11;").unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(
            std::fs::read_dir(root.path().join("data"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("migration-v11-"))
        );
        let op = enqueue(
            &mut store,
            &loc,
            MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        );
        store.pause_account(&loc.account, true).unwrap();
        assert!(store.claim_operation().unwrap().is_none());
        store.pause_account(&loc.account, false).unwrap();
        for _ in 0..3 {
            assert!(store.claim_operation().unwrap().is_some());
            store
                .finish_operation(&op.id, OperationState::Pending, None)
                .unwrap();
            store
                .connection
                .execute("UPDATE mail_operations SET retry_at=0", [])
                .unwrap();
        }
        assert_eq!(
            store.operation_by_id(&op.id).unwrap().state,
            OperationState::Failed
        );
        assert!(store.claim_operation().unwrap().is_none());
        store.retry_operation(&op.id).unwrap();
        assert!(store.claim_operation().unwrap().is_some());
    }
}
