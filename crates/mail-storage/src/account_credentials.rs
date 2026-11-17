use crate::{AccountId, AccountSettings, Error, Result, Store, mutations};
use mail_protocol::{Command, Reply, Request, valid_id};
use rusqlite::params;

/// 同一次数据库读取取得的配置与凭据引用；不含密码，不进入公开 IPC。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountVersion {
    pub account: AccountId,
    pub settings: AccountSettings,
    pub credential_key: AccountId,
}

impl Store {
    pub fn credential_referenced(&self, key: &AccountId) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_credentials WHERE credential_key=?1)",
            [&key.0],
            |row| row.get(0),
        )?)
    }
    pub fn account_version(&self, account: &AccountId) -> Result<AccountVersion> {
        let settings = self
            .account(account)?
            .ok_or(Error::Invalid("账号不存在或不是远端账号"))?;
        let credential_key = self.connection.query_row(
            "SELECT credential_key FROM account_credentials WHERE account_id=?1 AND active=1",
            [&account.0],
            |row| row.get::<_, String>(0),
        )?;
        Ok(AccountVersion {
            account: account.clone(),
            settings,
            credential_key: AccountId(credential_key),
        })
    }
    pub fn credential_keys(&self, account: &AccountId) -> Result<Vec<AccountId>> {
        let mut query = self.connection.prepare("SELECT credential_key FROM account_credentials WHERE account_id=?1 ORDER BY credential_key")?;
        Ok(query
            .query_map([&account.0], |row| Ok(AccountId(row.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn validate_account_commit(&self, baseline: Option<&AccountVersion>) -> Result<()> {
        match baseline {
            Some(version) if self.account_version(&version.account)? != *version => {
                Err(Error::Conflict)
            }
            Some(_) => Ok(()),
            None => Ok(()),
        }
    }
    /// 密钥库已确认写入新键之后调用；配置、活动引用与请求结果一起提交。
    pub fn commit_account_credentials(
        &mut self,
        request: Option<&Request>,
        account: &AccountId,
        settings: &AccountSettings,
        credential_key: &AccountId,
        baseline: Option<&AccountVersion>,
    ) -> Result<()> {
        if !valid_id(&account.0)
            || !valid_id(&credential_key.0)
            || credential_key == account
            || baseline
                .is_some_and(|v| &v.account != account || &v.credential_key == credential_key)
        {
            return Err(Error::Invalid("账号或凭据版本无效"));
        }
        if let Some(request) = request {
            match &request.command {
                Command::CommitAccount {
                    account: target, ..
                } if target.as_ref() == baseline.map(|v| &v.account) => (),
                _ => return Err(Error::Invalid("账号提交请求与目标不匹配")),
            }
        }
        // 仍由单一 Store 所在线程执行；不可变借用允许复用既有行写入与校验。
        let tx = self.connection.unchecked_transaction()?;
        self.validate_account_commit(baseline)?;
        if baseline.is_some() {
            self.update_account_row(account, settings)?;
        } else {
            self.insert_account_row(account, settings)?;
        }
        tx.execute(
            "UPDATE account_credentials SET active=0 WHERE account_id=?1",
            [&account.0],
        )?;
        tx.execute(
            "INSERT INTO account_credentials VALUES (?1,?2,1)",
            params![credential_key.0, account.0],
        )?;
        if let Some(request) = request {
            mutations::finish_on(&tx, request, &Ok(Reply::AccountSaved(account.clone())))?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MutationClaim, tests::settings};
    use mail_content::draft::Draft;
    use mail_protocol::{DraftDocument, MutationStatus};
    fn request(id: char, account: Option<AccountId>) -> Request {
        Request::new(
            id.to_string().repeat(32),
            Command::CommitAccount {
                check: "f".repeat(32),
                account,
            },
        )
    }
    #[test]
    fn schema9_upgrade_keeps_account_draft_and_legacy_credential_reference() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = Store::open(&data).unwrap();
        let account = store.create_account(&settings()).unwrap();
        let draft = store
            .create_draft(
                &account,
                &DraftDocument {
                    headers: Default::default(),
                    body: Draft::plain("保留草稿🙂").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        store.connection.execute_batch("DROP VIEW visible_folder_messages; DROP TABLE mail_operations; DROP TRIGGER account_default_options; DROP TABLE account_options; DROP TRIGGER account_legacy_credential; DROP TABLE account_credentials; PRAGMA user_version=9;").unwrap();
        drop(store);
        let store = Store::open(&data).unwrap();
        let version = store.account_version(&account).unwrap();
        assert_eq!(version.credential_key, account);
        assert_eq!(version.settings, settings());
        assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
        assert!(std::fs::read_dir(&data).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v9-")
        }));
    }
    #[test]
    fn account_reference_and_result_commit_together_and_stale_baseline_cannot_replace_them() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = Store::open(&data).unwrap();
        let account = store.create_account(&settings()).unwrap();
        let baseline = store.account_version(&account).unwrap();
        let key = AccountId("c".repeat(32));
        let request = request('a', Some(account.clone()));
        assert!(matches!(
            store.claim_mutation(&request).unwrap(),
            MutationClaim::Acquired
        ));
        let mut changed = settings();
        changed.imap.host = "new.example.test".into();
        // 最后一步写结果失败也必须回滚配置和凭据引用。
        store.connection.execute_batch("CREATE TRIGGER fail_result BEFORE UPDATE ON mutation_results BEGIN SELECT RAISE(ABORT,'disk failure'); END;").unwrap();
        assert!(
            store
                .commit_account_credentials(
                    Some(&request),
                    &account,
                    &changed,
                    &key,
                    Some(&baseline)
                )
                .is_err()
        );
        assert_eq!(store.account_version(&account).unwrap(), baseline);
        assert!(!store.credential_referenced(&key).unwrap());
        assert!(matches!(
            store.mutation_result(&request.id).unwrap(),
            MutationStatus::InProgress
        ));
        store
            .connection
            .execute_batch("DROP TRIGGER fail_result;")
            .unwrap();
        store
            .commit_account_credentials(Some(&request), &account, &changed, &key, Some(&baseline))
            .unwrap();
        let current = store.account_version(&account).unwrap();
        assert_eq!(current.settings, changed);
        assert_eq!(current.credential_key, key);
        assert!(
            store
                .credential_keys(&account)
                .unwrap()
                .contains(&baseline.credential_key)
        );
        assert!(matches!(
            store.commit_account_credentials(
                None,
                &account,
                &settings(),
                &AccountId("d".repeat(32)),
                Some(&baseline)
            ),
            Err(Error::Conflict)
        ));
        assert_eq!(store.account_version(&account).unwrap(), current);
        drop(store);
        let store = Store::open(&data).unwrap();
        assert!(
            matches!(store.mutation_result(&request.id).unwrap(),MutationStatus::Finished(result) if matches!(&*result,Ok(Reply::AccountSaved(id)) if id==&account))
        );
        assert_eq!(store.account_version(&account).unwrap(), current);
    }
    #[test]
    fn new_account_and_result_roll_back_as_one_transaction() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = AccountId("b".repeat(32));
        let key = AccountId("c".repeat(32));
        let request = request('a', None);
        store.claim_mutation(&request).unwrap();
        store.connection.execute_batch("CREATE TRIGGER fail_result BEFORE UPDATE ON mutation_results BEGIN SELECT RAISE(ABORT,'disk failure'); END;").unwrap();
        assert!(
            store
                .commit_account_credentials(Some(&request), &account, &settings(), &key, None)
                .is_err()
        );
        assert!(store.accounts().unwrap().is_empty());
        assert!(!store.credential_referenced(&key).unwrap());
        assert!(!store.credential_referenced(&account).unwrap());
        store
            .connection
            .execute_batch("DROP TRIGGER fail_result;")
            .unwrap();
        store
            .commit_account_credentials(Some(&request), &account, &settings(), &key, None)
            .unwrap();
        assert_eq!(store.account_version(&account).unwrap().credential_key, key);
        assert!(
            matches!(store.mutation_result(&request.id).unwrap(),MutationStatus::Finished(result) if matches!(&*result,Ok(Reply::AccountSaved(id)) if id==&account))
        );
    }
}
