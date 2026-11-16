pub use mail_core::{AttachmentInfo, OutgoingAttachment, OutgoingSnapshot};
use std::io::Read;

use mail_content::draft::{Draft, MAX_JSON};
use mail_core::{
    AccountId, AttachmentId, DraftHeaders, DraftId, ResendAuthorization, SendState, SendTaskId,
    ServerSettings, SmtpOutcome,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    Error, Result, Store, new_id,
    objects::{self, Object},
};

/// 本轮暂设单附件/总附件 25 MiB，最多 32 个；尚非 SMTP SIZE 上限协商。
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDocument {
    pub headers: DraftHeaders,
    pub body: Draft,
    pub attachments: Vec<AttachmentId>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDraft {
    pub id: DraftId,
    pub account: AccountId,
    pub revision: u32,
    pub document: DraftDocument,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendTask {
    pub id: SendTaskId,
    pub account: AccountId,
    pub source_draft: DraftId,
    pub source_revision: u32,
    pub sender: String,
    /// 冻结的收件人与主题；主题在 `recipients.subject`，不另存一份。
    pub recipients: DraftHeaders,
    pub created_at: i64,
    pub state: SendState,
    /// 固定诊断代码，不接受包含密码或服务器原始响应的错误字符串。
    pub detail: String,
    pub resend_of: Option<SendTaskId>,
    /// 只有副本确认保存或查重命中后才存在；不表示允许重发。
    pub copy: Option<SentCopy>,
}
/// 已保存副本的服务器位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentCopy {
    pub folder: String,
    pub uidvalidity: u32,
    /// 服务器未提供 APPENDUID 时为 None。
    pub uid: Option<u32>,
}
/// 副本保存结果；Failed/Uncertain 都保持 CopyPending，只能重试副本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    Saved {
        folder: String,
        uidvalidity: u32,
        uid: Option<u32>,
        existed: bool,
    },
    Failed {
        code: &'static str,
    },
    Uncertain {
        code: &'static str,
    },
}
/// 副本网络阶段的只读输入；调用前后都不改变发送任务状态。
#[derive(Debug)]
pub struct CopyReady {
    pub snapshot: OutgoingSnapshot,
    /// 上次记录或用户可沿用的文件夹；None 时由网络层定位。
    pub folder: Option<String>,
    pub imap: ServerSettings,
}
/// 只能由成功持久化 Sending 状态的 begin_smtp 返回；不可克隆或自行构造。
#[derive(Debug)]
pub struct SmtpAttempt {
    account: AccountId,
    task: SendTaskId,
    snapshot: OutgoingSnapshot,
}
impl SmtpAttempt {
    pub fn snapshot(&self) -> &OutgoingSnapshot {
        &self.snapshot
    }
    pub fn account(&self) -> &AccountId {
        &self.account
    }
    pub fn task(&self) -> &SendTaskId {
        &self.task
    }
}

impl Store {
    /// reader 来自后台已经打开的输入，不让前台提交任意路径。
    pub fn import_attachment(
        &mut self,
        account: &AccountId,
        name: &str,
        media_type: &str,
        reader: &mut impl Read,
    ) -> Result<AttachmentInfo> {
        if matches!(name, "." | "..")
            || name.is_empty()
            || name.len() > 1024
            || name.chars().any(char::is_control)
            || name.contains(['/', '\\'])
        {
            return Err(Error::Invalid("附件名称须为不含路径的可显示文件名"));
        }
        if media_type.len() > 128
            || !media_type.contains('/')
            || !media_type
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/!#$&^_.+-".contains(&b))
        {
            return Err(Error::Invalid("无效附件类型"));
        }
        let object = Object::write(
            &self.objects,
            account,
            "attachment",
            reader,
            MAX_ATTACHMENT_BYTES,
        )?;
        let result = (|| {
            let tx = self.connection.transaction()?;
            object.insert(&tx)?;
            tx.execute(
                "INSERT INTO attachments(id,account_id,name,media_type) VALUES (?1,?2,?3,?4)",
                params![object.id, account.0, name, media_type],
            )?;
            tx.commit()?;
            Ok(AttachmentInfo {
                id: AttachmentId(object.id.clone()),
                name: name.into(),
                media_type: media_type.into(),
                size: object.size,
            })
        })();
        if result.is_err() {
            self.cleanup_failed_object(&object.id);
        }
        result
    }

    pub fn attachment(&self, account: &AccountId, id: &AttachmentId) -> Result<AttachmentInfo> {
        Ok(self.connection.query_row("SELECT a.name,a.media_type,o.size FROM attachments a JOIN objects o ON o.id=a.id AND o.account_id=a.account_id WHERE a.id=?1 AND a.account_id=?2",params![id.0,account.0],|r|Ok(AttachmentInfo {id:id.clone(),name:r.get(0)?,media_type:r.get(1)?,size:r.get(2)?}))?)
    }

    pub fn create_draft(
        &mut self,
        account: &AccountId,
        document: &DraftDocument,
    ) -> Result<StoredDraft> {
        self.write_draft(account, &DraftId(new_id()?), None, document)
    }

    /// expected_revision 防止迟到的自动保存覆盖新草稿；版本冲突不静默覆盖。
    pub fn save_draft(
        &mut self,
        account: &AccountId,
        id: &DraftId,
        expected_revision: u32,
        document: &DraftDocument,
    ) -> Result<StoredDraft> {
        self.write_draft(account, id, Some(expected_revision), document)
    }

    fn write_draft(
        &mut self,
        account: &AccountId,
        id: &DraftId,
        expected: Option<u32>,
        document: &DraftDocument,
    ) -> Result<StoredDraft> {
        validate_headers(&document.headers, false)?;
        self.validate_attachments(account, &document.attachments)?;
        let body = document
            .body
            .to_json()
            .map_err(|_| Error::Invalid("草稿正文校验失败"))?;
        let object = Object::write(
            &self.objects,
            account,
            "body",
            &mut body.as_slice(),
            MAX_JSON as u64,
        )?;
        let result = (|| {
            let revision = expected
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Error::Conflict)?;
            let tx = self.connection.transaction()?;
            object.insert(&tx)?;
            let recipients = encode_recipients(&document.headers)?;
            if let Some(previous) = expected {
                let changed=tx.execute("UPDATE drafts SET revision=?1,subject=?2,recipients=?3,body_id=?4 WHERE id=?5 AND account_id=?6 AND revision=?7",params![revision,document.headers.subject,recipients,object.id,id.0,account.0,previous])?;
                if changed != 1 {
                    return Err(Error::Conflict);
                }
                tx.execute(
                    "DELETE FROM draft_attachments WHERE draft_id=?1 AND account_id=?2",
                    params![id.0, account.0],
                )?;
            } else {
                tx.execute("INSERT INTO drafts(id,account_id,revision,subject,recipients,body_id) VALUES (?1,?2,?3,?4,?5,?6)",params![id.0,account.0,revision,document.headers.subject,recipients,object.id])?;
            }
            for (n, attachment) in document.attachments.iter().enumerate() {
                tx.execute(
                    "INSERT INTO draft_attachments VALUES (?1,?2,?3,?4)",
                    params![id.0, account.0, n as u32, attachment.0],
                )?;
            }
            tx.commit()?;
            Ok(StoredDraft {
                id: id.clone(),
                account: account.clone(),
                revision,
                document: document.clone(),
            })
        })();
        if result.is_err() {
            self.cleanup_failed_object(&object.id);
        }
        result
    }

    pub fn draft(&self, account: &AccountId, id: &DraftId) -> Result<StoredDraft> {
        let (revision,subject,recipients,body):(u32,String,String,String)=self.connection.query_row("SELECT revision,subject,recipients,body_id FROM drafts WHERE id=?1 AND account_id=?2",params![id.0,account.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let bytes = objects::read(
            &self.objects,
            &self.connection,
            account,
            &body,
            "body",
            MAX_JSON as u64,
        )?;
        let body =
            Draft::from_json(&bytes).map_err(|_| Error::Invalid("草稿正文损坏，保留原文件"))?;
        let attachments = attachment_ids(
            &self.connection,
            "draft_attachments",
            "draft_id",
            account,
            &id.0,
        )?;
        self.validate_attachments(account, &attachments)?;
        Ok(StoredDraft {
            id: id.clone(),
            account: account.clone(),
            revision,
            document: DraftDocument {
                headers: decode_headers(subject, &recipients)?,
                body,
                attachments,
            },
        })
    }

    pub fn draft_ids(&self, account: &AccountId) -> Result<Vec<DraftId>> {
        let mut query = self
            .connection
            .prepare("SELECT id FROM drafts WHERE account_id=?1 ORDER BY id")?;
        Ok(query
            .query_map([&account.0], |r| Ok(DraftId(r.get(0)?)))?
            .collect::<std::result::Result<_, _>>()?)
    }

    pub fn discard_draft(
        &mut self,
        account: &AccountId,
        id: &DraftId,
        expected_revision: u32,
    ) -> Result<()> {
        let n = self.connection.execute(
            "DELETE FROM drafts WHERE id=?1 AND account_id=?2 AND revision=?3",
            params![id.0, account.0, expected_revision],
        )?;
        if n != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// 发送预览不创建任务；校验当前持久草稿、附件副本和账号。
    pub fn draft_send_snapshot(
        &self,
        account: &AccountId,
        id: &DraftId,
        revision: u32,
    ) -> Result<OutgoingSnapshot> {
        let draft = self.draft(account, id)?;
        if draft.revision != revision {
            return Err(Error::Conflict);
        }
        let sender = self
            .account(account)?
            .ok_or(Error::Invalid("请先关联发件账号"))?
            .address;
        Ok(OutgoingSnapshot {
            message_id: format!("<{}@qingmail.invalid>", id.0),
            date_unix: unix_now()?,
            sender,
            headers: draft.document.headers,
            plain: draft.document.body.plain_export(),
            html: draft.document.body.html(),
            attachments: self.read_attachments(account, &draft.document.attachments)?,
        })
    }
    pub fn draft_tasks(&self, account: &AccountId, id: &DraftId) -> Result<Vec<SendTask>> {
        let mut query=self.connection.prepare("SELECT id FROM send_tasks WHERE account_id=?1 AND source_draft=?2 ORDER BY created_at DESC,rowid DESC LIMIT 50")?;
        let ids = query
            .query_map(params![account.0, id.0], |r| Ok(SendTaskId(r.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.task(account, id)).collect()
    }

    /// 仅由用户明确发送动作调用。双击同一草稿版本返回同一个任务，不重复提交。
    pub fn enqueue_draft(
        &mut self,
        account: &AccountId,
        id: &DraftId,
        expected_revision: u32,
    ) -> Result<SendTaskId> {
        let draft = self.draft(account, id)?;
        if draft.revision != expected_revision {
            return Err(Error::Conflict);
        }
        validate_headers(&draft.document.headers, true)?;
        let sender = self
            .account(account)?
            .ok_or(Error::Invalid("账号不存在"))?
            .address;
        if !valid_mailbox(&sender) {
            return Err(Error::Invalid("发件人地址无效"));
        }
        self.read_attachments(account, &draft.document.attachments)?;
        if let Some(id)=self.connection.query_row("SELECT id FROM send_tasks WHERE source_draft=?1 AND source_revision=?2 AND account_id=?3 AND resend_of IS NULL",params![id.0,expected_revision,account.0],|r|r.get::<_,String>(0)).optional()? {return Ok(SendTaskId(id));}
        let task = SendTaskId(new_id()?);
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO send_tasks(id,account_id,source_draft,source_revision,sender,subject,recipients,body_id,state,created_at) SELECT ?1,d.account_id,d.id,d.revision,a.address,d.subject,d.recipients,d.body_id,'pending',?5 FROM drafts d JOIN accounts a ON a.id=d.account_id WHERE d.id=?2 AND d.account_id=?3 AND d.revision=?4",params![task.0,id.0,account.0,expected_revision,unix_now()?])?;
        tx.execute("INSERT INTO task_attachments SELECT ?1,account_id,ordinal,attachment_id FROM draft_attachments WHERE draft_id=?2 AND account_id=?3",params![task.0,id.0,account.0])?;
        tx.commit()?;
        Ok(task)
    }

    pub fn task(&self, account: &AccountId, id: &SendTaskId) -> Result<SendTask> {
        type TaskRow = (
            String,
            u32,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<u32>,
            Option<u32>,
            String,
            String,
            String,
            i64,
        );
        let (draft,revision,state,detail,parent,folder,epoch,uid,sender,subject,recipients,created_at): TaskRow = self.connection.query_row("SELECT source_draft,source_revision,state,detail,resend_of,copy_folder,copy_uidvalidity,copy_uid,sender,subject,recipients,created_at FROM send_tasks WHERE id=?1 AND account_id=?2",params![id.0,account.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?)))?;
        let copy = match (folder, epoch) {
            (Some(folder), Some(uidvalidity)) if uidvalidity != 0 => Some(SentCopy {
                folder,
                uidvalidity,
                uid,
            }),
            (None, None) => None,
            _ => return Err(Error::Invalid("发送副本位置不完整")),
        };
        let headers = decode_headers(subject, &recipients)?;
        Ok(SendTask {
            id: id.clone(),
            account: account.clone(),
            source_draft: DraftId(draft),
            source_revision: revision,
            sender,
            recipients: headers,
            created_at,
            state: parse_state(&state)?,
            detail,
            resend_of: parent.map(SendTaskId),
            copy,
        })
    }

    /// 任务入口读取；有界倒序，不触发网络或自动重试。
    pub fn task_list(&self, account: &AccountId) -> Result<Vec<SendTask>> {
        let mut query=self.connection.prepare("SELECT id FROM send_tasks WHERE account_id=?1 ORDER BY created_at DESC,rowid DESC LIMIT 200")?;
        let ids = query
            .query_map([&account.0], |r| Ok(SendTaskId(r.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.task(account, id)).collect()
    }

    pub fn task_ids(&self, account: &AccountId) -> Result<Vec<SendTaskId>> {
        let mut query = self
            .connection
            .prepare("SELECT id FROM send_tasks WHERE account_id=?1 ORDER BY id")?;
        Ok(query
            .query_map([&account.0], |r| Ok(SendTaskId(r.get(0)?)))?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// 返回任务快照给只读详情，不改变任何发送状态。
    pub fn task_snapshot(&self, account: &AccountId, id: &SendTaskId) -> Result<OutgoingSnapshot> {
        let (sender,subject,recipients,body,date_unix):(String,String,String,String,i64)=self.connection.query_row("SELECT sender,subject,recipients,body_id,created_at FROM send_tasks WHERE id=?1 AND account_id=?2",params![id.0,account.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let bytes = objects::read(
            &self.objects,
            &self.connection,
            account,
            &body,
            "body",
            MAX_JSON as u64,
        )?;
        let body =
            Draft::from_json(&bytes).map_err(|_| Error::Invalid("发送正文损坏，禁止提交"))?;
        let ids = attachment_ids(
            &self.connection,
            "task_attachments",
            "task_id",
            account,
            &id.0,
        )?;
        let headers = decode_headers(subject, &recipients)?;
        validate_headers(&headers, true)?;
        if !valid_mailbox(&sender) {
            return Err(Error::Invalid("发送快照发件人无效"));
        }
        Ok(OutgoingSnapshot {
            message_id: format!("<{}@qingmail.invalid>", id.0),
            date_unix,
            sender,
            headers,
            plain: body.plain_export(),
            html: body.html(),
            attachments: self.read_attachments(account, &ids)?,
        })
    }

    /// 先校验完整快照，再持久化 sending，成功后才向协议层返回提交许可。
    pub fn begin_smtp(&mut self, account: &AccountId, id: &SendTaskId) -> Result<SmtpAttempt> {
        if self.task(account, id)?.state != SendState::Pending {
            return Err(Error::Conflict);
        }
        let snapshot = self.task_snapshot(account, id)?;
        self.transition(account, id, "pending", "sending", "")?;
        Ok(SmtpAttempt {
            account: account.clone(),
            task: id.clone(),
            snapshot,
        })
    }

    /// 消费提交许可；SMTP 接受后必先进入 copy_pending，不能返回可发送队列。
    pub fn finish_smtp(&mut self, attempt: SmtpAttempt, outcome: SmtpOutcome) -> Result<()> {
        let (state, detail) = match outcome {
            SmtpOutcome::Accepted => ("copy_pending", ""),
            SmtpOutcome::DefinitelyRejected => ("failed", "smtp_rejected"),
            SmtpOutcome::Uncertain => ("uncertain", "smtp_result_unknown"),
        };
        self.transition(&attempt.account, &attempt.task, "sending", state, detail)
    }

    pub fn begin_copy(&self, account: &AccountId, id: &SendTaskId) -> Result<OutgoingSnapshot> {
        if self.task(account, id)?.state != SendState::CopyPending {
            return Err(Error::Conflict);
        }
        self.task_snapshot(account, id)
    }

    /// 读取冻结快照、上次副本文件夹与 IMAP 配置；只在 CopyPending 时可用。
    pub fn copy_ready(&self, account: &AccountId, id: &SendTaskId) -> Result<CopyReady> {
        let task = self.task(account, id)?;
        if task.state != SendState::CopyPending {
            return Err(Error::Conflict);
        }
        let imap = self
            .account(account)?
            .ok_or(Error::Invalid("账号不存在"))?
            .imap;
        Ok(CopyReady {
            snapshot: self.task_snapshot(account, id)?,
            folder: task.copy.map(|copy| copy.folder),
            imap,
        })
    }

    /// 保存成功才进入 Sent；失败或不确定都保持 CopyPending，等待用户只重试副本。
    pub fn finish_copy(
        &mut self,
        account: &AccountId,
        id: &SendTaskId,
        outcome: CopyOutcome,
    ) -> Result<()> {
        match outcome {
            CopyOutcome::Saved {
                folder,
                uidvalidity,
                uid,
                existed,
            } => {
                if folder.is_empty()
                    || folder.len() > 4096
                    || folder.chars().any(char::is_control)
                    || uidvalidity == 0
                    || uid == Some(0)
                {
                    return Err(Error::Invalid("已发送副本位置无效"));
                }
                let detail = if existed { "copy_already_present" } else { "" };
                if self.connection.execute(
                    "UPDATE send_tasks SET state='sent',detail=?1,copy_folder=?2,copy_uidvalidity=?3,copy_uid=?4 WHERE id=?5 AND account_id=?6 AND state='copy_pending'",
                    params![detail, folder, uidvalidity, uid, id.0, account.0],
                )? != 1
                {
                    return Err(Error::Conflict);
                }
            }
            CopyOutcome::Failed { code } | CopyOutcome::Uncertain { code } => {
                if !known_copy_code(code) {
                    return Err(Error::Invalid("未知副本诊断代码"));
                }
                if self.connection.execute(
                    "UPDATE send_tasks SET detail=?1 WHERE id=?2 AND account_id=?3 AND state='copy_pending'",
                    params![code, id.0, account.0],
                )? != 1
                {
                    return Err(Error::Conflict);
                }
            }
        }
        Ok(())
    }

    /// 副本仍需保存的任务；用于任务入口，不触发任何网络或自动重试。
    pub fn copy_pending_tasks(&self, account: &AccountId) -> Result<Vec<SendTask>> {
        let mut query=self.connection.prepare("SELECT id FROM send_tasks WHERE account_id=?1 AND state='copy_pending' ORDER BY created_at DESC,rowid DESC LIMIT 50")?;
        let ids = query
            .query_map([&account.0], |r| Ok(SendTaskId(r.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.task(account, id)).collect()
    }

    pub fn cancel_pending(&mut self, account: &AccountId, id: &SendTaskId) -> Result<()> {
        self.transition(
            account,
            id,
            "pending",
            "failed",
            "cancelled_before_submission",
        )
    }

    /// 只标记本地停止处理，不宣称撤回，也不把不确定改成未发送。
    pub fn stop_uncertain(&mut self, account: &AccountId, id: &SendTaskId) -> Result<()> {
        self.transition(
            account,
            id,
            "uncertain",
            "uncertain",
            "user_stopped_processing",
        )
    }

    /// 已明确失败/结果不确定时，用户确认会创建新任务；旧记录不被改写。
    pub fn resend_task(
        &mut self,
        account: &AccountId,
        id: &SendTaskId,
        _authorization: ResendAuthorization,
    ) -> Result<SendTaskId> {
        let previous = self.task(account, id)?;
        if !matches!(previous.state, SendState::Failed | SendState::Uncertain) {
            return Err(Error::Conflict);
        }
        if let Some(existing) = self
            .connection
            .query_row(
                "SELECT id FROM send_tasks WHERE account_id=?1 AND resend_of=?2",
                params![account.0, id.0],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(SendTaskId(existing));
        }
        self.task_snapshot(account, id)?;
        let next = SendTaskId(new_id()?);
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO send_tasks(id,account_id,source_draft,source_revision,sender,subject,recipients,body_id,state,resend_of,created_at) SELECT ?1,account_id,source_draft,source_revision,sender,subject,recipients,body_id,'pending',id,?4 FROM send_tasks WHERE id=?2 AND account_id=?3",params![next.0,id.0,account.0,unix_now()?])?;
        tx.execute("INSERT INTO task_attachments SELECT ?1,account_id,ordinal,attachment_id FROM task_attachments WHERE task_id=?2 AND account_id=?3",params![next.0,id.0,account.0])?;
        tx.commit()?;
        Ok(next)
    }

    fn transition(
        &mut self,
        account: &AccountId,
        id: &SendTaskId,
        from: &str,
        to: &str,
        detail: &str,
    ) -> Result<()> {
        if self.connection.execute(
            "UPDATE send_tasks SET state=?1,detail=?2 WHERE id=?3 AND account_id=?4 AND state=?5",
            params![to, detail, id.0, account.0, from],
        )? != 1
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    fn cleanup_failed_object(&self, id: &str) {
        // COMMIT 的 I/O 错误可能无法确定最终结果；仅确认无记录后才删文件。
        let exists = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE id=?1)",
            [id],
            |r| r.get::<_, bool>(0),
        );
        if matches!(exists, Ok(false)) {
            self.objects.remove(id);
        }
    }

    fn validate_attachments(&self, account: &AccountId, ids: &[AttachmentId]) -> Result<()> {
        if ids.len() > MAX_ATTACHMENTS {
            return Err(Error::Invalid("附件数量超过上限"));
        }
        let mut size = 0;
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                return Err(Error::Invalid("重复附件引用"));
            }
            size += u64::from(self.attachment(account, id)?.size);
            if size > MAX_ATTACHMENT_BYTES {
                return Err(Error::Invalid("附件总大小超过上限"));
            }
        }
        Ok(())
    }
    fn read_attachments(
        &self,
        account: &AccountId,
        ids: &[AttachmentId],
    ) -> Result<Vec<OutgoingAttachment>> {
        self.validate_attachments(account, ids)?;
        ids.iter()
            .map(|id| {
                Ok(OutgoingAttachment {
                    info: self.attachment(account, id)?,
                    bytes: objects::read(
                        &self.objects,
                        &self.connection,
                        account,
                        &id.0,
                        "attachment",
                        MAX_ATTACHMENT_BYTES,
                    )?,
                })
            })
            .collect()
    }
}

pub(crate) fn recover_interrupted(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction()?;
    tx.execute("UPDATE send_tasks SET state='failed',detail='interrupted_before_submission' WHERE state='pending'",[])?;
    tx.execute("UPDATE send_tasks SET state='uncertain',detail='interrupted_during_submission' WHERE state='sending'",[])?;
    tx.commit()?;
    Ok(())
}

fn attachment_ids(
    connection: &Connection,
    table: &str,
    owner_column: &str,
    account: &AccountId,
    owner: &str,
) -> Result<Vec<AttachmentId>> {
    // table/owner_column 仅来自本模块的固定字面值，不接收请求内容。
    let mut query=connection.prepare(&format!("SELECT attachment_id FROM {table} WHERE {owner_column}=?1 AND account_id=?2 ORDER BY ordinal"))?;
    Ok(query
        .query_map(params![owner, account.0], |r| Ok(AttachmentId(r.get(0)?)))?
        .collect::<std::result::Result<_, _>>()?)
}
fn validate_headers(headers: &DraftHeaders, submitting: bool) -> Result<()> {
    if headers.subject.len() > 4096 || headers.subject.chars().any(char::is_control) {
        return Err(Error::Invalid("主题包含控制字符或超过上限"));
    }
    let count = headers.to.len() + headers.cc.len() + headers.bcc.len();
    if count > 100 || submitting && count == 0 {
        return Err(Error::Invalid("收件人为空或超过 100 个"));
    }
    for address in headers.to.iter().chain(&headers.cc).chain(&headers.bcc) {
        if address.is_empty() || address.len() > 320 || address.chars().any(char::is_control) {
            return Err(Error::Invalid("收件人字段无效"));
        }
        // 编辑中的不完整地址可以保存；提交前仅接受独立 mailbox，完整 RFC 检查留给协议构建层。
        if submitting && !valid_mailbox(address) {
            return Err(Error::Invalid("发送前请修正收件人地址"));
        }
    }
    Ok(())
}
fn encode_recipients(headers: &DraftHeaders) -> Result<String> {
    serde_json::to_string(&(&headers.to, &headers.cc, &headers.bcc))
        .map_err(|_| Error::Invalid("收件人无法保存"))
}
fn decode_headers(subject: String, recipients: &str) -> Result<DraftHeaders> {
    if recipients.len() > 128 * 1024 {
        return Err(Error::Invalid("收件人数据超限"));
    }
    let (to, cc, bcc) =
        serde_json::from_str(recipients).map_err(|_| Error::Invalid("收件人数据损坏"))?;
    let headers = DraftHeaders {
        subject,
        to,
        cc,
        bcc,
    };
    validate_headers(&headers, false)?;
    Ok(headers)
}
/// 只接受固定诊断代码，避免把服务器原文或凭据写进任务记录。
fn known_copy_code(code: &str) -> bool {
    matches!(
        code,
        "copy_invalid"
            | "copy_no_sent_folder"
            | "copy_connection_failed"
            | "copy_timeout"
            | "copy_tls_failed"
            | "copy_auth_failed"
            | "copy_protocol_failed"
            | "copy_limit"
            | "copy_search_failed"
            | "copy_append_rejected"
            | "copy_result_unknown"
            | "copy_prepare_failed"
            | "copy_already_present"
    )
}
fn parse_state(value: &str) -> Result<SendState> {
    match value {
        "pending" => Ok(SendState::Pending),
        "sending" => Ok(SendState::Sending),
        "sent" => Ok(SendState::Sent),
        "failed" => Ok(SendState::Failed),
        "uncertain" => Ok(SendState::Uncertain),
        "copy_pending" => Ok(SendState::CopyPending),
        _ => Err(Error::Invalid("未知发送状态")),
    }
}

#[cfg(test)]
mod tests;

fn valid_mailbox(address: &str) -> bool {
    !address.is_empty()
        && address.len() <= 320
        && address.matches('@').count() == 1
        && !address.starts_with('@')
        && !address.ends_with('@')
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
        && !address.contains(['<', '>', ',', ';'])
}
fn unix_now() -> Result<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .ok_or(Error::Invalid("系统时间无效"))
}
