//! 前台只写 Secret Service 临时项，IPC 只传随机引用；不覆盖正式账号凭据。
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STAGING_TTL: Duration = Duration::from_secs(15 * 60);
const APPLICATION: &str = "org.qingmail.QingMail";
const PURPOSE: &str = "credential-staging-v1";

pub trait CredentialStaging: Send + Sync {
    fn stage(&self, credentials: &Credentials) -> Result<String, VaultError>;
    fn load_staged(&self, reference: &str) -> Result<Credentials, VaultError>;
    fn discard_staged(&self, reference: &str) -> Result<(), VaultError>;
    fn cleanup_staged(&self) -> Result<u64, VaultError>;
}
fn valid_reference(reference: &str) -> bool {
    reference.len() == 32
        && reference
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn now() -> Result<u64, VaultError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| VaultError::Invalid)
}
fn attributes(reference: &str) -> HashMap<&str, &str> {
    HashMap::from([
        ("application", APPLICATION),
        ("purpose", PURPOSE),
        ("ref", reference),
    ])
}
fn metadata(attributes: &HashMap<String, String>, reference: &str) -> Result<u64, VaultError> {
    if !valid_reference(reference)
        || attributes.get("application").map(String::as_str) != Some(APPLICATION)
        || attributes.get("purpose").map(String::as_str) != Some(PURPOSE)
        || attributes.get("ref").map(String::as_str) != Some(reference)
        || attributes.contains_key("account")
    {
        return Err(VaultError::Invalid);
    }
    attributes
        .get("created")
        .and_then(|v| v.parse().ok())
        .ok_or(VaultError::Invalid)
}
fn encode_staged(reference: &str, created: u64, credentials: &Credentials) -> Zeroizing<Vec<u8>> {
    let mut value = Zeroizing::new(Vec::new());
    value.extend_from_slice(reference.as_bytes());
    value.extend_from_slice(&created.to_be_bytes());
    value.extend_from_slice(&encode(credentials));
    value
}
fn decode_staged(
    reference: &str,
    attributes: &HashMap<String, String>,
    value: &[u8],
    now: u64,
) -> Result<Credentials, VaultError> {
    let created = metadata(attributes, reference)?;
    if !(44..=({ crate::microsoft::MAX_SECRET_BYTES + OAUTH_MAGIC.len() + 40 }))
        .contains(&value.len())
        || &value[..32] != reference.as_bytes()
        || value[32..40] != created.to_be_bytes()
        || created > now
    {
        return Err(VaultError::Invalid);
    }
    if now - created >= STAGING_TTL.as_secs() {
        return Err(VaultError::Expired);
    }
    decode(&value[40..])
}
impl CredentialStaging for SystemVault {
    fn stage(&self, credentials: &Credentials) -> Result<String, VaultError> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| VaultError::OperationFailed)?;
        let reference = format!("{:032x}", u128::from_be_bytes(random));
        let created = now()?;
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
            let created_text = created.to_string();
            let mut attributes = attributes(&reference);
            attributes.insert("created", &created_text);
            let value = encode_staged(&reference, created, credentials);
            let item = collection
                .create_item(
                    "QingMail 临时邮箱凭据",
                    attributes,
                    &value,
                    false,
                    "application/octet-stream",
                )
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            let read_back = Zeroizing::new(
                item.get_secret()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?,
            );
            if *read_back != *value {
                return Err(VaultError::OperationFailed);
            }
            Ok(reference)
        })
    }
    fn load_staged(&self, reference: &str) -> Result<Credentials, VaultError> {
        if !valid_reference(reference) {
            return Err(VaultError::Invalid);
        }
        bounded(async {
            let service = connect().await?;
            let found = service
                .search_items(attributes(reference))
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            if !found.locked.is_empty() {
                return Err(VaultError::Locked);
            }
            if found.unlocked.len() > 1 {
                return Err(VaultError::Ambiguous);
            }
            let item = found.unlocked.first().ok_or(VaultError::Missing)?;
            let attributes = item
                .get_attributes()
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            let value = Zeroizing::new(
                item.get_secret()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?,
            );
            decode_staged(reference, &attributes, &value, now()?)
        })
    }
    fn discard_staged(&self, reference: &str) -> Result<(), VaultError> {
        if !valid_reference(reference) {
            return Err(VaultError::Invalid);
        }
        bounded(async {
            let service = connect().await?;
            let found = service
                .search_items(attributes(reference))
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            if !found.locked.is_empty() {
                return Err(VaultError::Locked);
            }
            if found.unlocked.len() > 1 {
                return Err(VaultError::Ambiguous);
            }
            if let Some(item) = found.unlocked.first() {
                metadata(
                    &item
                        .get_attributes()
                        .await
                        .map_err(|_| VaultError::OperationFailed)?,
                    reference,
                )?;
                item.delete()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?;
            }
            Ok(())
        })
    }
    fn cleanup_staged(&self) -> Result<u64, VaultError> {
        bounded(async {
            let service = connect().await?;
            let found = service
                .search_items(HashMap::from([
                    ("application", APPLICATION),
                    ("purpose", PURPOSE),
                ]))
                .await
                .map_err(|_| VaultError::OperationFailed)?;
            let now = now()?;
            let mut deleted = 0;
            // 只清理当前应用的过期临时项；锁定/未知/正式项保留，不弹出解锁提示。
            for item in found.unlocked.iter().take(256) {
                let attributes = item
                    .get_attributes()
                    .await
                    .map_err(|_| VaultError::OperationFailed)?;
                let Some(reference) = attributes.get("ref") else {
                    continue;
                };
                let Ok(created) = metadata(&attributes, reference) else {
                    continue;
                };
                if now
                    .checked_sub(created)
                    .is_some_and(|age| age >= STAGING_TTL.as_secs())
                {
                    item.delete()
                        .await
                        .map_err(|_| VaultError::OperationFailed)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staging_binds_reference_metadata_and_expiration_without_touching_formal_items() {
        let reference = "a".repeat(32);
        let credentials =
            Credentials::new("synthetic-imap".into(), "synthetic-smtp".into()).unwrap();
        let value = encode_staged(&reference, 1000, &credentials);
        let mut attrs: HashMap<String, String> = attributes(&reference)
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        attrs.insert("created".into(), "1000".into());
        let decoded = decode_staged(&reference, &attrs, &value, 1001).unwrap();
        assert_eq!(*decoded.imap, *credentials.imap);
        assert_eq!(*decoded.smtp, *credentials.smtp);
        assert!(matches!(
            decode_staged(&reference, &attrs, &value, 1900),
            Err(VaultError::Expired)
        ));
        assert!(matches!(
            decode_staged(&reference, &attrs, &value, 999),
            Err(VaultError::Invalid)
        ));
        assert!(decode_staged(&"b".repeat(32), &attrs, &value, 1001).is_err());
        attrs.insert("created".into(), "1001".into());
        assert!(decode_staged(&reference, &attrs, &value, 1001).is_err());
        attrs.insert("created".into(), "1000".into());
        attrs.insert("account".into(), "formal-account".into());
        assert!(metadata(&attrs, &reference).is_err());
        attrs.remove("account");
        attrs.insert("purpose".into(), "imap-smtp-v1".into());
        assert!(metadata(&attrs, &reference).is_err());
        for value in [&b""[..], &value[..39], &[0; 8237]] {
            assert!(decode_staged(&reference, &attrs, value, 1001).is_err());
        }
        assert_eq!(
            SystemVault.load_staged("../not-an-item").err(),
            Some(VaultError::Invalid)
        );
        assert_eq!(
            SystemVault.discard_staged("/a/path").err(),
            Some(VaultError::Invalid)
        );
    }
}
