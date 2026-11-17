//! P3 单账号业务命令。在线操作与 P3-004 持久离线队列分开。
use crate::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BusinessOperation {
    ReadServerDraft {
        account: AccountId,
        source: String,
    },
    ScanServerDrafts {
        account: AccountId,
    },
    ImportServerDraft {
        account: AccountId,
        folder: String,
        uidvalidity: u32,
        uid: u32,
    },
    RefreshCatalog {
        account: AccountId,
    },
    SyncFolder {
        account: AccountId,
        name: String,
        before: Option<u32>,
        history: bool,
    },
    Read {
        location: MailUid,
        folder: String,
    },
    Flags {
        account: AccountId,
        name: String,
        epoch: u32,
        uids: Vec<u32>,
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    Move {
        account: AccountId,
        name: String,
        epoch: u32,
        uids: Vec<u32>,
        destination: String,
    },
    TaskContent {
        account: AccountId,
        task: SendTaskId,
    },
    BrowserCopy {
        content: String,
    },
    RemoteImages {
        content: String,
    },
    ClearCopies,
    RemoveAccount {
        account: AccountId,
    },
}
impl BusinessOperation {
    pub fn writes(&self) -> bool {
        matches!(
            self,
            Self::Flags { .. } | Self::Move { .. } | Self::RemoveAccount { .. }
        )
    }
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::ReadServerDraft { source, .. } if !valid_id(source) => {
                Err(ProtocolError::Invalid)
            }
            Self::Flags { epoch, uids, .. } | Self::Move { epoch, uids, .. }
                if *epoch == 0
                    || uids.is_empty()
                    || uids.len() > MAX_BATCH
                    || uids.contains(&0) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::ImportServerDraft {
                folder,
                uidvalidity,
                uid,
                ..
            } if folder.is_empty()
                || folder.len() > 4096
                || folder.chars().any(char::is_control)
                || *uidvalidity == 0
                || *uid == 0 =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::Read { location, .. } if location.uid == 0 || location.uidvalidity == 0 => {
                Err(ProtocolError::Invalid)
            }
            Self::BrowserCopy { content } | Self::RemoteImages { content }
                if !valid_id(content) =>
            {
                Err(ProtocolError::Invalid)
            }
            _ => Ok(()),
        }
    }
}
