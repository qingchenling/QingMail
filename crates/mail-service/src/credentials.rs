use mail_core::AccountId;
use secret_service::{EncryptionType, SecretService};
use std::{collections::HashMap, fmt, time::Duration};
use zeroize::Zeroizing;
pub mod staging;

/// 不实现 Debug/Clone，避免意外日志和无意扩散；不承诺第三方库内存副本全部清零。
pub struct Credentials {
    pub imap: Zeroizing<String>,
    pub smtp: Zeroizing<String>,
}
impl Credentials {
    pub fn new(imap: String, smtp: String) -> Result<Self, VaultError> {
        let imap = Zeroizing::new(imap);
        let smtp = Zeroizing::new(smtp);
        if [&imap, &smtp]
            .iter()
            .any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err(VaultError::Invalid);
        }
        Ok(Self { imap, smtp })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultError {
    Unavailable,
    Locked,
    Missing,
    Ambiguous,
    Invalid,
    OperationFailed,
    Timeout,
    Expired,
}
impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "系统密钥库不可用，请配置 Secret Service",
            Self::Locked => "密钥库已锁定，请在系统中解锁后重试",
            Self::Missing => "密钥库中未找到账号凭据",
            Self::Ambiguous => "密钥库存在重复项，请处理后重试",
            Self::Invalid => "凭据格式无效",
            Self::Timeout => "密钥库操作超时；保存结果可能未知，请检查密钥库后重试",
            Self::Expired => "临时凭据已过期，请重新输入密码并检查连接",
            Self::OperationFailed => "密钥库操作失败，未回退明文文件",
        })
    }
}
impl std::error::Error for VaultError {}
pub trait CredentialVault {
    fn status(&self) -> Result<(), VaultError>;
    fn save(&self, account: &AccountId, credentials: &Credentials) -> Result<(), VaultError>;
    fn load(&self, account: &AccountId) -> Result<Credentials, VaultError>;
    fn delete(&self, account: &AccountId) -> Result<(), VaultError>;
}
pub struct SystemVault;
async fn connect() -> Result<SecretService<'static>, VaultError> {
    let connection = zbus::connection::Builder::session()
        .map_err(|_| VaultError::Unavailable)?
        .method_timeout(Duration::from_secs(5))
        .build()
        .await
        .map_err(|_| VaultError::Unavailable)?;
    SecretService::connect_with_existing(EncryptionType::Dh, connection)
        .await
        .map_err(|_| VaultError::Unavailable)
}
fn attributes(account: &AccountId) -> HashMap<&str, &str> {
    HashMap::from([
        ("application", "org.qingmail.QingMail"),
        ("account", account.0.as_str()),
        ("purpose", "imap-smtp-v1"),
    ])
}
fn encode(credentials: &Credentials) -> Zeroizing<Vec<u8>> {
    let mut value = Zeroizing::new(Vec::with_capacity(
        credentials.imap.len() + credentials.smtp.len() + 4,
    ));
    value.extend_from_slice(&(credentials.imap.len() as u32).to_be_bytes());
    value.extend_from_slice(credentials.imap.as_bytes());
    value.extend_from_slice(credentials.smtp.as_bytes());
    value
}
fn decode(value: &[u8]) -> Result<Credentials, VaultError> {
    if value.len() < 4 || value.len() > 8196 {
        return Err(VaultError::Invalid);
    }
    let length =
        u32::from_be_bytes(value[..4].try_into().map_err(|_| VaultError::Invalid)?) as usize;
    if length > value.len() - 4 {
        return Err(VaultError::Invalid);
    }
    let imap = std::str::from_utf8(&value[4..4 + length])
        .map_err(|_| VaultError::Invalid)?
        .to_owned();
    let smtp = std::str::from_utf8(&value[4 + length..])
        .map_err(|_| VaultError::Invalid)?
        .to_owned();
    Credentials::new(imap, smtp)
}
/// 取消等待不会推断远端写入已回滚；超时不能据此插入账号。
fn bounded<T>(
    operation: impl std::future::Future<Output = Result<T, VaultError>>,
) -> Result<T, VaultError> {
    bounded_for(operation, Duration::from_secs(15))
}
fn bounded_for<T>(
    operation: impl std::future::Future<Output = Result<T, VaultError>>,
    duration: Duration,
) -> Result<T, VaultError> {
    futures_lite::future::block_on(futures_lite::future::or(operation, async {
        async_io::Timer::after(duration).await;
        Err(VaultError::Timeout)
    }))
}
impl CredentialVault for SystemVault {
    fn status(&self) -> Result<(), VaultError> {
        bounded(async {
            let service = connect().await?;
            let collection = service
                .get_default_collection()
                .await
                .map_err(|_| VaultError::Unavailable)?;
            if collection
                .is_locked()
                .await
                .map_err(|_| VaultError::OperationFailed)?
            {
                return Err(VaultError::Locked);
            }
            Ok(())
        })
    }
    fn save(&self, account: &AccountId, credentials: &Credentials) -> Result<(), VaultError> {
        bounded(async {
            let service = connect().await?;
            let collection = service
                .get_default_collection()
                .await
                .map_err(|_| VaultError::Unavailable)?;
            if collection
                .is_locked()
                .await
                .map_err(|_| VaultError::OperationFailed)?
            {
                return Err(VaultError::Locked);
            }
            // 一个密钥项同时保存 IMAP/SMTP，避免只保存半套凭据。
            let encoded = encode(credentials);
            let item = collection
                .create_item(
                    "QingMail 邮箱凭据",
                    attributes(account),
                    &encoded,
                    true,
                    "application/octet-stream",
                )
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            let read_back = Zeroizing::new(
                item.get_secret()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?,
            );
            if *read_back != *encoded {
                return Err(VaultError::OperationFailed);
            }
            Ok(())
        })
    }
    fn load(&self, account: &AccountId) -> Result<Credentials, VaultError> {
        bounded(async {
            let service = connect().await?;
            let found = service
                .search_items(attributes(account))
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            if !found.locked.is_empty() {
                return Err(VaultError::Locked);
            }
            if found.unlocked.len() > 1 {
                return Err(VaultError::Ambiguous);
            }
            let item = found.unlocked.first().ok_or(VaultError::Missing)?;
            let value = Zeroizing::new(
                item.get_secret()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?,
            );
            decode(&value)
        })
    }
    fn delete(&self, account: &AccountId) -> Result<(), VaultError> {
        bounded(async {
            let service = connect().await?;
            let found = service
                .search_items(attributes(account))
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            if !found.locked.is_empty() {
                return Err(VaultError::Locked);
            }
            if found.unlocked.len() > 1 {
                return Err(VaultError::Ambiguous);
            }
            if let Some(item) = found.unlocked.first() {
                item.delete()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_keyring_operation_is_cancelled_at_deadline() {
        let dropped = std::cell::Cell::new(false);
        struct Guard<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let guard = Guard(&dropped);
        let result = bounded_for(
            async move {
                let _guard = guard;
                std::future::pending::<Result<(), VaultError>>().await
            },
            Duration::from_millis(10),
        );
        assert_eq!(result, Err(VaultError::Timeout));
        assert!(dropped.get());
    }
    #[test]
    fn secret_pair_roundtrip_and_corrupt_bounds() {
        let c = Credentials::new("中文 password".into(), "smtp different".into()).unwrap();
        let decoded = decode(&encode(&c)).unwrap();
        assert_eq!(*decoded.imap, *c.imap);
        assert_eq!(*decoded.smtp, *c.smtp);
        for value in [&b""[..], &b"\xff\xff\xff\xffx"[..], &b"\0\0\0\x01a"[..]] {
            assert!(decode(value).is_err());
        }
        assert!(Credentials::new("x\r\ny".into(), "safe".into()).is_err());
    }
}
