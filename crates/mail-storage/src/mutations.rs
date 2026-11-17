//! IPC 幂等记录与业务操作分离：先可靠取得执行权，完成结果落盘后才应答。
//! 发生进程中断时保留不确定记录，不能以“没有最终响应”推断可重新执行。
use crate::{Error, Result, Store};
use mail_protocol::{MAX_CONTROL_BYTES, MutationStatus, Reply, Request, RpcError, valid_id};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub enum MutationClaim {
    Acquired,
    Existing(MutationStatus),
}
fn fingerprint(request: &Request) -> Result<String> {
    request
        .validate()
        .map_err(|_| Error::Invalid("IPC 请求无效"))?;
    let bytes =
        serde_json::to_vec(&request.command).map_err(|_| Error::Invalid("IPC 命令无法编码"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn decode(state: &str, result: Option<String>) -> Result<MutationStatus> {
    match (state, result) {
        ("in_progress", None) => Ok(MutationStatus::InProgress),
        ("uncertain", None) => Ok(MutationStatus::Uncertain),
        ("finished", Some(result)) => serde_json::from_str(&result)
            .map(|value| MutationStatus::Finished(Box::new(value)))
            .map_err(|_| Error::Invalid("IPC 结果记录损坏，禁止重复执行")),
        _ => Err(Error::Invalid("IPC 结果状态损坏，禁止重复执行")),
    }
}
impl Store {
    pub fn claim_mutation(&mut self, request: &Request) -> Result<MutationClaim> {
        let fingerprint = fingerprint(request)?;
        let tx = self.connection.transaction()?;
        let previous: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT fingerprint,state,result FROM mutation_results WHERE request_id=?1",
                [&request.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((previous, state, result)) = previous {
            if previous != fingerprint {
                return Err(Error::Conflict);
            }
            return Ok(MutationClaim::Existing(decode(&state, result)?));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Invalid("系统时间无效"))?
            .as_secs() as i64;
        tx.execute(
            "INSERT INTO mutation_results VALUES (?1,?2,'in_progress',NULL,?3)",
            params![request.id, fingerprint, now],
        )?;
        tx.commit()?;
        Ok(MutationClaim::Acquired)
    }
    /// 即便业务已成功，完成结果未写盘时也不向前台声称可重试或保存已确认。
    pub fn finish_mutation(
        &mut self,
        request: &Request,
        result: &std::result::Result<Reply, RpcError>,
    ) -> Result<()> {
        finish_on(&self.connection, request, result)
    }
    pub fn mutation_result(&self, id: &str) -> Result<MutationStatus> {
        if !valid_id(id) {
            return Err(Error::Invalid("IPC 请求标识无效"));
        }
        let row: Option<(String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT state,result FROM mutation_results WHERE request_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((state, result)) => decode(&state, result),
            None => Ok(MutationStatus::NotFound),
        }
    }
}
pub(super) fn finish_on(
    connection: &rusqlite::Connection,
    request: &Request,
    result: &std::result::Result<Reply, RpcError>,
) -> Result<()> {
    let fingerprint = fingerprint(request)?;
    let encoded = serde_json::to_string(result).map_err(|_| Error::Invalid("IPC 结果无法编码"))?;
    if encoded.len() > MAX_CONTROL_BYTES - 256 {
        return Err(Error::Invalid("IPC 结果超过控制帧限制"));
    }
    let updated = connection.execute(
        "UPDATE mutation_results SET state='finished',result=?1 WHERE request_id=?2 AND fingerprint=?3 AND state='in_progress'",
        params![encoded,request.id,fingerprint],
    )?;
    if updated != 1 {
        return Err(Error::Conflict);
    }
    Ok(())
}
pub(super) fn recover(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute(
        "UPDATE mutation_results SET state='uncertain' WHERE state='in_progress'",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_protocol::Command;
    #[test]
    fn durable_claim_response_loss_parameter_conflict_and_crash_recovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let request = Request::new(
            "a".repeat(32),
            Command::DiscardSelectedDraft { account: None },
        );
        let unfinished = Request::new(
            "b".repeat(32),
            Command::DiscardSelectedDraft { account: None },
        );
        let mut store = Store::open(&path).unwrap();
        assert!(matches!(
            store.claim_mutation(&request).unwrap(),
            MutationClaim::Acquired
        ));
        assert!(matches!(
            store.claim_mutation(&request).unwrap(),
            MutationClaim::Existing(MutationStatus::InProgress)
        ));
        let conflict = Request::new(request.id.clone(), Command::Accounts);
        assert!(matches!(
            store.claim_mutation(&conflict),
            Err(Error::Conflict)
        ));
        assert!(
            store
                .finish_mutation(&conflict, &Ok(Reply::Acknowledged))
                .is_err()
        );
        store
            .finish_mutation(&request, &Ok(Reply::Count(3)))
            .unwrap();
        assert!(
            store
                .finish_mutation(&request, &Ok(Reply::Count(4)))
                .is_err()
        );
        assert!(matches!(
            store.claim_mutation(&unfinished).unwrap(),
            MutationClaim::Acquired
        ));
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert!(
            matches!(store.claim_mutation(&request).unwrap(),MutationClaim::Existing(MutationStatus::Finished(value)) if matches!(*value,Ok(Reply::Count(3))))
        );
        assert!(matches!(
            store.claim_mutation(&unfinished).unwrap(),
            MutationClaim::Existing(MutationStatus::Uncertain)
        ));
        assert!(matches!(
            store.mutation_result(&"c".repeat(32)).unwrap(),
            MutationStatus::NotFound
        ));
        assert!(store.mutation_result("../file").is_err());
    }
    #[test]
    fn schema8_upgrade_preserves_drafts_and_keeps_private_backup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut store = Store::open(&path).unwrap();
        let account = store.create_account(&crate::tests::settings()).unwrap();
        let draft = store
            .create_draft(
                &account,
                &mail_protocol::DraftDocument {
                    headers: Default::default(),
                    body: mail_content::draft::Draft::plain("P2 草稿🙂").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        store
            .connection
            .execute_batch("DROP TRIGGER account_legacy_credential; DROP TABLE account_credentials; DROP TABLE mutation_results; PRAGMA user_version=8;")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
        assert_eq!(
            store.account(&account).unwrap(),
            Some(crate::tests::settings())
        );
        assert!(std::fs::read_dir(&path).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v8-")
        }));
        assert!(matches!(
            store.mutation_result(&"d".repeat(32)).unwrap(),
            MutationStatus::NotFound
        ));
    }
}
