//! 存储线程内的 IPC 分派；复用既有草稿函数及 Store 校验，不暴露任意 SQL/路径。
use super::*;
use mail_protocol::{
    Command as Rpc, ErrorCode, MutationStatus, PROTOCOL_VERSION, Reply, Response, RpcError,
};
use mail_storage::MutationClaim;

fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
pub(super) fn storage_error(e: mail_storage::Error) -> RpcError {
    match e {
        mail_storage::Error::Conflict => error(ErrorCode::Conflict, e.to_string()),
        _ => error(ErrorCode::Storage, e.to_string()),
    }
}
fn local_error(message: String) -> RpcError {
    error(ErrorCode::Storage, message)
}
pub(crate) const COMMANDS: &[mail_protocol::CommandName] = &[
    mail_protocol::CommandName::DraftSyncStatus,
    mail_protocol::CommandName::FlushDraftSync,
    mail_protocol::CommandName::ServerDrafts,
    mail_protocol::CommandName::DraftSyncFolder,
    mail_protocol::CommandName::ConvertServerDraft,
    mail_protocol::CommandName::DraftPage,
    mail_protocol::CommandName::TaskPage,
    mail_protocol::CommandName::Accounts,
    mail_protocol::CommandName::EnqueueOperation,
    mail_protocol::CommandName::Operations,
    mail_protocol::CommandName::RetryOperation,
    mail_protocol::CommandName::ResolveOperation,
    mail_protocol::CommandName::AccountOptions,
    mail_protocol::CommandName::PauseSync,
    mail_protocol::CommandName::SetDefaultAccount,
    mail_protocol::CommandName::UnifiedInbox,
    mail_protocol::CommandName::AccountImpact,
    mail_protocol::CommandName::Catalog,
    mail_protocol::CommandName::Folders,
    mail_protocol::CommandName::FolderCache,
    mail_protocol::CommandName::Search,
    mail_protocol::CommandName::OpenDraft,
    mail_protocol::CommandName::NewDraft,
    mail_protocol::CommandName::Drafts,
    mail_protocol::CommandName::SelectDraft,
    mail_protocol::CommandName::SaveDraft,
    mail_protocol::CommandName::DiscardSelectedDraft,
    mail_protocol::CommandName::AttachmentInfos,
    mail_protocol::CommandName::Tasks,
    mail_protocol::CommandName::DraftTasks,
    mail_protocol::CommandName::StopUncertain,
    mail_protocol::CommandName::MutationResult,
];
fn mutates(command: &Rpc) -> bool {
    matches!(
        command,
        Rpc::FlushDraftSync { .. }
            | Rpc::DraftSyncFolder { .. }
            | Rpc::ConvertServerDraft { .. }
            | Rpc::RetryOperation { .. }
            | Rpc::ResolveOperation { .. }
            | Rpc::SetDefaultAccount { .. }
            | Rpc::PauseSync {
                account: Some(_),
                ..
            }
            | Rpc::OpenDraft { .. }
            | Rpc::NewDraft { .. }
            | Rpc::SelectDraft { .. }
            | Rpc::SaveDraft { .. }
            | Rpc::DiscardSelectedDraft { .. }
            | Rpc::StopUncertain { .. }
    )
}
pub(super) fn dispatch(store: &mut Store, request: mail_protocol::Request) -> Response {
    let response_id = request.id.clone();
    let result = (|| {
        request.validate().map_err(|e| {
            error(
                if matches!(e, mail_protocol::ProtocolError::Version) {
                    ErrorCode::VersionMismatch
                } else {
                    ErrorCode::InvalidRequest
                },
                e.to_string(),
            )
        })?;
        if !COMMANDS.contains(&request.command.name()) {
            return Err(error(ErrorCode::Unsupported, "此命令尚未接入独立后台"));
        }
        if matches!(request.command, Rpc::EnqueueOperation { .. }) {
            let mut trash = std::collections::BTreeMap::new();
            if let Rpc::EnqueueOperation {
                locations,
                operation: mail_protocol::MailOperation::Delete,
            } = &request.command
            {
                for loc in locations {
                    let catalog = store.remote_folders(&loc.account).map_err(storage_error)?;
                    if let Some(name) = mail_service::folders::trash(&catalog, None)
                        && let Some(folder) = store
                            .folders(&loc.account)
                            .map_err(storage_error)?
                            .into_iter()
                            .find(|f| f.remote_name == name)
                    {
                        trash.insert(loc.account.0.clone(), folder.id);
                    }
                }
            }
            return store
                .enqueue_operations_with_trash(&request, &trash)
                .map_err(storage_error);
        }
        if !mutates(&request.command) {
            return execute(store, request.command);
        }
        match store.claim_mutation(&request).map_err(storage_error)? {
            MutationClaim::Existing(MutationStatus::Finished(result)) => return *result,
            MutationClaim::Existing(MutationStatus::InProgress) => {
                return Err(error(ErrorCode::Busy, "请求已在执行；请查询原请求结果"));
            }
            MutationClaim::Existing(_) => {
                return Err(error(
                    ErrorCode::Uncertain,
                    "请求结果尚不确定；请检查已保存状态，不能重新执行原操作",
                ));
            }
            MutationClaim::Acquired => (),
        }
        let result = execute(store, request.command.clone());
        store.finish_mutation(&request, &result).map_err(|_| {
            error(
                ErrorCode::Uncertain,
                "操作结果未确认写盘，请查询原请求并重新读取业务状态，不能重复提交",
            )
        })?;
        result
    })();
    Response {
        version: PROTOCOL_VERSION,
        id: response_id,
        result,
    }
}
fn execute(store: &mut Store, command: Rpc) -> Result<Reply, RpcError> {
    Ok(match command {
        Rpc::DraftSyncStatus { account } => {
            Reply::DraftSyncStatus(store.draft_sync_status(&account).map_err(storage_error)?)
        }
        Rpc::FlushDraftSync { account } => {
            store.flush_draft_sync(&account).map_err(storage_error)?;
            Reply::Acknowledged
        }

        Rpc::ServerDrafts { account } => Reply::ServerDrafts {
            folder: store.server_draft_folder(&account).map_err(storage_error)?,
            items: store
                .server_draft_sources(&account)
                .map_err(storage_error)?,
        },
        Rpc::DraftSyncFolder { account, folder } => {
            store
                .set_server_draft_folder(&account, &folder)
                .map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::ConvertServerDraft { account, source } => {
            let draft = store
                .convert_server_draft(&account, &source)
                .map_err(storage_error)?;
            Reply::Draft(draft)
        }

        Rpc::DraftPage { account, after } => {
            let account = editor_account(store, account).map_err(local_error)?;
            let ids = store
                .draft_ids_page(&account, after.as_deref())
                .map_err(storage_error)?;
            let next = if ids.len() > 1 {
                Some(ids[0].0.clone())
            } else {
                None
            };
            let items = ids
                .iter()
                .take(1)
                .map(|id| store.draft(&account, id))
                .collect::<mail_storage::Result<_>>()
                .map_err(storage_error)?;
            Reply::DraftPage { items, next }
        }
        Rpc::TaskPage { account, after } => {
            let ids = store
                .task_ids_page(&account, after.as_deref())
                .map_err(storage_error)?;
            let next = if ids.len() > 32 {
                Some(ids[31].0.clone())
            } else {
                None
            };
            let items = ids
                .iter()
                .take(32)
                .map(|id| store.task(&account, id))
                .collect::<mail_storage::Result<_>>()
                .map_err(storage_error)?;
            Reply::TaskPage { items, next }
        }
        Rpc::AccountOptions => Reply::AccountOptions {
            default: store.default_account().map_err(storage_error)?,
            paused: store
                .accounts()
                .map_err(storage_error)?
                .into_iter()
                .filter_map(|(id, _)| match store.account_paused(&id) {
                    Ok(true) => Some(Ok(id)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<mail_storage::Result<_>>()
                .map_err(storage_error)?,
        },
        Rpc::SetDefaultAccount { account } => {
            store.set_default_account(&account).map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::PauseSync {
            account: Some(account),
            paused,
        } => {
            store
                .pause_account(&account, paused)
                .map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::UnifiedInbox { before, limit } => Reply::Inbox(
            store
                .unified_inbox(before.as_ref(), limit)
                .map_err(storage_error)?,
        ),
        Rpc::Operations { account } => {
            Reply::Operations(store.operations(account.as_ref()).map_err(storage_error)?)
        }
        Rpc::RetryOperation { operation } => {
            store.retry_operation(&operation).map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::ResolveOperation {
            operation,
            resolution,
        } => {
            store
                .resolve_operation(&operation, resolution)
                .map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::Accounts => Reply::Accounts(store.accounts().map_err(storage_error)?),
        Rpc::AccountImpact { account } => {
            if store.account(&account).map_err(storage_error)?.is_none() {
                return Err(error(ErrorCode::NotFound, "账号不存在"));
            }
            Reply::AccountImpact(store.account_impact(&account).map_err(storage_error)?)
        }
        Rpc::Catalog { account } => Reply::Catalog {
            folders: store.remote_folders(&account).map_err(storage_error)?,
            synced_at: store.catalog_synced_at(&account).map_err(storage_error)?,
        },
        Rpc::Folders { account } => Reply::Folders(store.folders(&account).map_err(storage_error)?),
        Rpc::FolderCache {
            account,
            name,
            before,
        } => Reply::FolderCache(
            store
                .folder_cache(&account, &name, before)
                .map_err(storage_error)?,
        ),
        Rpc::Search { account, query } => Reply::Search(
            store
                .search_scope(account.as_ref(), &query)
                .map_err(storage_error)?,
        ),
        Rpc::OpenDraft { account, initial } => {
            let account = editor_account(store, account).map_err(local_error)?;
            Reply::Draft(open_for(store, &account, initial).map_err(local_error)?)
        }
        Rpc::NewDraft { account, document } => {
            let account = editor_account(store, account).map_err(local_error)?;
            Reply::Draft(create_document(store, &account, document).map_err(local_error)?)
        }
        Rpc::Drafts { account } => {
            let account = editor_account(store, account).map_err(local_error)?;
            Reply::Drafts(
                store
                    .draft_ids(&account)
                    .map_err(storage_error)?
                    .iter()
                    .map(|id| store.draft(&account, id))
                    .collect::<mail_storage::Result<_>>()
                    .map_err(storage_error)?,
            )
        }
        Rpc::SelectDraft { account, draft } => {
            let account = editor_account(store, account).map_err(local_error)?;
            let stored = store.draft(&account, &draft).map_err(storage_error)?;
            store
                .select_editor_draft(&account, &draft)
                .map_err(storage_error)?;
            Reply::Draft(stored)
        }
        Rpc::SaveDraft {
            account,
            draft,
            revision,
            document,
        } => Reply::Draft(
            store
                .save_draft(&account, &draft, revision, &document)
                .map_err(storage_error)?,
        ),
        Rpc::DiscardSelectedDraft { account } => {
            let account = editor_account(store, account).map_err(local_error)?;
            store
                .discard_selected_draft(&account)
                .map_err(storage_error)?;
            Reply::Acknowledged
        }
        Rpc::AttachmentInfos { account, ids } => Reply::Attachments(
            ids.iter()
                .map(|id| store.attachment(&account, id))
                .collect::<mail_storage::Result<_>>()
                .map_err(storage_error)?,
        ),
        Rpc::Tasks { account } => Reply::Tasks(
            store
                .task_ids(&account)
                .map_err(storage_error)?
                .iter()
                .map(|id| store.task(&account, id))
                .collect::<mail_storage::Result<_>>()
                .map_err(storage_error)?,
        ),
        Rpc::DraftTasks { account, draft } => {
            Reply::Tasks(store.draft_tasks(&account, &draft).map_err(storage_error)?)
        }
        Rpc::StopUncertain { account, task } => {
            store
                .stop_uncertain(&account, &task)
                .map_err(storage_error)?;
            Reply::Task(store.task(&account, &task).map_err(storage_error)?)
        }
        Rpc::MutationResult { mutation } => {
            Reply::Mutation(store.mutation_result(&mutation).map_err(storage_error)?)
        }
        _ => {
            return Err(error(
                ErrorCode::Unsupported,
                "此命令或查询范围尚未接入独立后台",
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(id: char, command: Rpc) -> mail_protocol::Request {
        mail_protocol::Request::new(id.to_string().repeat(32), command)
    }
    fn document(text: &str) -> DraftDocument {
        DraftDocument {
            headers: Default::default(),
            body: Draft::plain(text).unwrap(),
            attachments: vec![],
        }
    }
    fn draft(response: Response) -> StoredDraft {
        match response.result.unwrap() {
            Reply::Draft(d) => d,
            _ => panic!("expected draft"),
        }
    }
    #[test]
    fn lost_response_replay_and_restart_do_not_duplicate_or_overwrite_drafts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let create = request(
            'a',
            Rpc::NewDraft {
                account: None,
                document: document("first🙂"),
            },
        );
        let first = draft(receive(worker.client().rpc(create.clone())).unwrap());
        assert_eq!(
            draft(receive(worker.client().rpc(create.clone())).unwrap()),
            first
        );
        let save = request(
            'b',
            Rpc::SaveDraft {
                account: first.account.clone(),
                draft: first.id.clone(),
                revision: first.revision,
                document: document("saved🙂"),
            },
        );
        let saved = draft(receive(worker.client().rpc(save.clone())).unwrap());
        assert_eq!(saved.revision, first.revision + 1);
        let stale = receive(worker.client().rpc(request(
            'c',
            Rpc::SaveDraft {
                account: first.account.clone(),
                draft: first.id.clone(),
                revision: first.revision,
                document: document("stale"),
            },
        )))
        .unwrap();
        assert!(matches!(
            stale.result,
            Err(RpcError {
                code: ErrorCode::Conflict,
                ..
            })
        ));
        worker.stop();
        let mut worker = StorageWorker::start(Some(path)).unwrap();
        assert_eq!(draft(receive(worker.client().rpc(save)).unwrap()), saved);
        assert_eq!(draft(receive(worker.client().rpc(create)).unwrap()), first); // 已完成请求返回当时结果，UI随后读取最新版本。
        assert_eq!(receive(worker.client().list()).unwrap(), vec![saved]);
        let changed = receive(worker.client().rpc(request(
            'a',
            Rpc::NewDraft {
                account: None,
                document: document("different"),
            },
        )))
        .unwrap();
        assert!(matches!(
            changed.result,
            Err(RpcError {
                code: ErrorCode::Conflict,
                ..
            })
        ));
        worker.stop();
    }
    #[test]
    fn unsupported_or_wrong_version_cannot_create_send_permissions_or_mutate_data() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let mut invalid = request(
            'a',
            Rpc::NewDraft {
                account: None,
                document: document("reject"),
            },
        );
        invalid.version = 99;
        assert!(matches!(
            dispatch(&mut store, invalid).result,
            Err(RpcError {
                code: ErrorCode::VersionMismatch,
                ..
            })
        ));
        assert!(matches!(
            store.mutation_result(&"a".repeat(32)).unwrap(),
            MutationStatus::NotFound
        ));
        let send = request(
            'b',
            Rpc::ConfirmSend {
                review: "c".repeat(32),
                allow_empty_subject: true,
            },
        );
        assert!(matches!(
            dispatch(&mut store, send).result,
            Err(RpcError {
                code: ErrorCode::Unsupported,
                ..
            })
        ));
        assert!(matches!(
            store.mutation_result(&"b".repeat(32)).unwrap(),
            MutationStatus::NotFound
        ));
        assert!(store.accounts().unwrap().is_empty());
    }
}
