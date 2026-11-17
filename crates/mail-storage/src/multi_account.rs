use crate::*;
use mail_protocol::{InboxCursor, MessagePage};

impl Store {
    pub fn default_account(&self) -> Result<Option<AccountId>> {
        Ok(self.connection.query_row(
            "SELECT a.id FROM accounts a LEFT JOIN account_options o ON o.account_id=a.id WHERE a.kind='remote' ORDER BY COALESCE(o.is_default,0) DESC,a.id LIMIT 1",
            [], |r| Ok(AccountId(r.get(0)?)),
        ).optional()?)
    }
    pub fn set_default_account(&mut self, account: &AccountId) -> Result<()> {
        if self.account(account)?.is_none() {
            return Err(Error::Invalid("账号不存在"));
        }
        let tx = self.connection.transaction()?;
        tx.execute(
            "UPDATE account_options SET is_default=0 WHERE is_default=1",
            [],
        )?;
        tx.execute("INSERT INTO account_options(account_id,is_default) VALUES (?1,1) ON CONFLICT(account_id) DO UPDATE SET is_default=1", [&account.0])?;
        tx.commit()?;
        Ok(())
    }
    pub fn account_paused(&self, account: &AccountId) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT paused FROM account_options WHERE account_id=?1",
                [&account.0],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }
    pub fn pause_account(&mut self, account: &AccountId, paused: bool) -> Result<()> {
        if self.account(account)?.is_none() {
            return Err(Error::Invalid("账号不存在"));
        }
        self.connection.execute("INSERT INTO account_options(account_id,paused) VALUES (?1,?2) ON CONFLICT(account_id) DO UPDATE SET paused=excluded.paused", params![account.0,paused])?;
        Ok(())
    }
    /// 日期相同时以账号、内部邮件 ID 排序，跨账号不按 Message-ID 合并。
    pub fn unified_inbox(&self, before: Option<&InboxCursor>, limit: u32) -> Result<MessagePage> {
        if !(1..=200).contains(&limit) {
            return Err(Error::Invalid("分页数量须为 1–200"));
        }
        let mut query = self.connection.prepare(
            "SELECT m.id,fm.account_id,fm.folder_id,fm.uidvalidity,fm.uid,m.subject,m.sender,m.received_at,fm.seen,fm.flagged FROM visible_folder_messages fm JOIN messages m ON m.id=fm.message_id AND m.account_id=fm.account_id JOIN folders f ON f.id=fm.folder_id AND f.account_id=fm.account_id WHERE f.remote_name COLLATE NOCASE='INBOX' AND (?1 IS NULL OR (m.received_at,fm.account_id,m.id)<(?1,?2,?3)) ORDER BY m.received_at DESC,fm.account_id DESC,m.id DESC LIMIT ?4"
        )?;
        let mut messages = query
            .query_map(
                params![
                    before.map(|c| c.received_at),
                    before.map(|c| &c.account.0),
                    before.map(|c| &c.message.0),
                    limit + 1
                ],
                |r| {
                    Ok(CachedMessage {
                        id: MessageId(r.get(0)?),
                        location: MailUid {
                            account: AccountId(r.get(1)?),
                            folder_id: r.get(2)?,
                            uidvalidity: r.get(3)?,
                            uid: r.get(4)?,
                        },
                        envelope: MessageEnvelope {
                            subject: r.get(5)?,
                            sender: r.get(6)?,
                            received_at: r.get(7)?,
                        },
                        seen: r.get(8)?,
                        flagged: r.get(9)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let more = messages.len() > limit as usize;
        messages.truncate(limit as usize);
        let next = messages.last().filter(|_| more).map(|m| InboxCursor {
            received_at: m.envelope.received_at,
            account: m.location.account.clone(),
            message: m.id.clone(),
        });
        Ok(MessagePage { messages, next })
    }
}
