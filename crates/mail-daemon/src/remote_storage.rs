//! 前台异步存储适配：保留编辑器的有界队列接口，实际数据只由 daemon 持有。
use super::*;
use mail_protocol::{Command as Rpc, Reply, transport::Client};
impl StorageWorker {
    /// 不创建 Store、SQLite 连接或网络同步器。
    pub fn connect(client: Client) -> std::io::Result<Self> {
        let (queue, receiver) = mpsc::sync_channel(16);
        let remote = client.clone();
        let thread = thread::Builder::new()
            .name("qingmail-ipc-storage".into())
            .spawn(move || run_remote(remote, receiver))?;
        Ok(Self {
            client: StorageClient {
                queue,
                remote: Some(client),
            },
            thread: Some(thread),
        })
    }
}
fn call(client: &Client, command: Rpc) -> StorageResult<Reply> {
    use mail_protocol::{
        MutationStatus, Request,
        transport::{ClientError, request_id},
    };
    let write = matches!(
        command,
        Rpc::OpenDraft { .. }
            | Rpc::NewDraft { .. }
            | Rpc::SelectDraft { .. }
            | Rpc::SaveDraft { .. }
            | Rpc::DiscardSelectedDraft { .. }
            | Rpc::StopUncertain { .. }
            | Rpc::FinishUpload { .. }
    );
    let request = Request::new(request_id().map_err(|e| e.to_string())?, command);
    match client.call(&request) {
        Ok(reply) => return Ok(reply),
        Err(ClientError::Remote(error))
            if !write
                || !matches!(
                    error.code,
                    mail_protocol::ErrorCode::Uncertain | mail_protocol::ErrorCode::Busy
                ) =>
        {
            return Err(error.message);
        }
        Err(error) if !write => return Err(error.to_string()),
        Err(_) => (),
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match client.request(Rpc::MutationResult {
            mutation: request.id.clone(),
        }) {
            Ok(Reply::Mutation(MutationStatus::Finished(result))) => {
                return (*result).map_err(|e| e.message);
            }
            Ok(Reply::Mutation(MutationStatus::InProgress)) => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => break,
        }
    }
    Err(format!(
        "保存结果尚未确定，请重新读取已保存内容；不会自动重放（请求{}）",
        request.id
    ))
}
fn run_remote(client: Client, receiver: Receiver<Request>) {
    macro_rules! answer {
        ($reply:expr,$command:expr,$pattern:pat=>$value:expr) => {{
            let result = call(&client, $command).and_then(|reply| match reply {
                $pattern => Ok($value),
                _ => Err("IPC响应类型不符，请重连后台".into()),
            });
            let _ = $reply.send(result);
        }};
    }
    while let Ok(request) = receiver.recv() {
        match request {
            Request::Stop => break,
            Request::Accounts { reply } => answer!(reply,Rpc::Accounts,Reply::Accounts(v)=>v),
            Request::AccountImpact { account, reply } => {
                answer!(reply,Rpc::AccountImpact{account},Reply::AccountImpact(v)=>v)
            }
            Request::RemoveAccount { account, reply } => {
                let result = crate::client::run(
                    &client,
                    mail_protocol::BusinessOperation::RemoveAccount { account },
                )
                .and_then(|r| match r {
                    Reply::AccountImpact(v) => Ok(v),
                    _ => Err("账号移除响应无效".into()),
                });
                let _ = reply.send(result);
            }
            Request::Catalog { account, reply } => {
                answer!(reply,Rpc::Catalog{account},Reply::Catalog{folders,synced_at}=>(folders,synced_at))
            }
            Request::FolderCache {
                account,
                name,
                before,
                reply,
            } => answer!(reply,Rpc::FolderCache{account,name,before},Reply::FolderCache(v)=>v),
            Request::Folders { account, reply } => {
                answer!(reply,Rpc::Folders{account},Reply::Folders(v)=>v)
            }
            Request::Search {
                account,
                query,
                reply,
            } => answer!(reply,Rpc::Search{account:Some(account),query},Reply::Search(v)=>v),
            Request::Open {
                account,
                initial,
                reply,
            } => answer!(reply,Rpc::OpenDraft{account,initial},Reply::Draft(v)=>v),
            Request::New {
                account,
                body,
                reply,
            } => {
                answer!(reply,Rpc::NewDraft{account,document:DraftDocument{headers:Default::default(),body,attachments:vec![]}},Reply::Draft(v)=>v)
            }
            Request::NewDocument {
                account,
                document,
                reply,
            } => {
                answer!(reply,Rpc::NewDraft{account:Some(account),document:*document},Reply::Draft(v)=>v)
            }
            Request::Save {
                previous,
                body,
                reply,
            } => {
                let mut document = previous.document;
                document.body = body;
                answer!(reply,Rpc::SaveDraft{account:previous.account,draft:previous.id,revision:previous.revision,document},Reply::Draft(v)=>v);
            }
            Request::SaveDocument {
                previous,
                document,
                reply,
            } => {
                answer!(reply,Rpc::SaveDraft{account:previous.account,draft:previous.id,revision:previous.revision,document},Reply::Draft(v)=>v)
            }
            Request::List { account, reply } => {
                let result = (|| {
                    let mut items = vec![];
                    let mut after = None;
                    loop {
                        let Reply::DraftPage { items: page, next } = call(
                            &client,
                            Rpc::DraftPage {
                                account: account.clone(),
                                after: after.clone(),
                            },
                        )?
                        else {
                            return Err("草稿分页响应无效".into());
                        };
                        items.extend(page);
                        if next.is_none() {
                            return Ok(items);
                        }
                        if next == after {
                            return Err("草稿分页游标无效".into());
                        }
                        after = next;
                    }
                })();
                let _ = reply.send(result);
            }
            Request::Select { account, id, reply } => {
                answer!(reply,Rpc::SelectDraft{account,draft:id},Reply::Draft(v)=>v)
            }
            Request::DiscardSelectedDraft { account, reply } => {
                answer!(reply,Rpc::DiscardSelectedDraft{account},Reply::Acknowledged=>())
            }
            Request::AttachmentInfos {
                account,
                ids,
                reply,
            } => answer!(reply,Rpc::AttachmentInfos{account,ids},Reply::Attachments(v)=>v),
            Request::Tasks { account, reply } => {
                let result = (|| {
                    let mut items = vec![];
                    let mut after = None;
                    loop {
                        let Reply::TaskPage { items: page, next } = call(
                            &client,
                            Rpc::TaskPage {
                                account: account.clone(),
                                after: after.clone(),
                            },
                        )?
                        else {
                            return Err("任务分页响应无效".into());
                        };
                        items.extend(page);
                        if next.is_none() {
                            items.sort_by_key(|t| std::cmp::Reverse(t.created_at));
                            return Ok(items);
                        }
                        if next == after {
                            return Err("任务分页游标无效".into());
                        }
                        after = next;
                    }
                })();
                let _ = reply.send(result);
            }
            Request::DraftTasks { account, id, reply } => {
                answer!(reply,Rpc::DraftTasks{account,draft:id},Reply::Tasks(v)=>v)
            }
            Request::StopUncertain {
                account,
                task,
                reply,
            } => answer!(reply,Rpc::StopUncertain{account,task},Reply::Task(v)=>v),
            Request::SelectedDraftBody { account, reply } => {
                let result =
                    call(&client, Rpc::SelectedDraftBody { account }).and_then(
                        |reply| match reply {
                            Reply::OptionalResource(Some(resource)) => {
                                let result = client
                                    .read_resource(&resource, mail_content::draft::MAX_JSON as u64)
                                    .map(Some)
                                    .map_err(|e| e.to_string());
                                let _ = call(
                                    &client,
                                    Rpc::CancelResource {
                                        resource: resource.id,
                                    },
                                );
                                result
                            }
                            Reply::OptionalResource(None) => Ok(None),
                            _ => Err("草稿资源响应无效".into()),
                        },
                    );
                let _ = reply.send(result);
            }
            Request::ReviewSend { draft, reply } => {
                let result = call(
                    &client,
                    Rpc::ReviewSend {
                        account: draft.account.clone(),
                        draft: draft.id.clone(),
                        revision: draft.revision,
                    },
                )
                .and_then(|response| match response {
                    Reply::SendPreview(preview) => {
                        Ok(crate::sending::SendReview::from_remote(preview, &draft))
                    }
                    _ => Err("发送预览响应类型无效".into()),
                });
                let _ = reply.send(result);
            }
            Request::ImportAttachment {
                account,
                path,
                reply,
            } => {
                let _ = reply.send(upload_selected(&client, account, &path));
            }
            Request::Rpc { request, reply } => {
                let result = client
                    .call(&request)
                    .map(|result| mail_protocol::Response {
                        version: mail_protocol::PROTOCOL_VERSION,
                        id: request.id,
                        result: Ok(result),
                    })
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            // 内部数据库写入/发送许可没有前台协议入口。接收方得到明确的通道断开错误。
            _ => (),
        }
    }
}
fn upload_selected(
    client: &Client,
    account: AccountId,
    path: &std::path::Path,
) -> StorageResult<mail_storage::AttachmentInfo> {
    use std::io::Read;
    let (name, mut file) = super::selected_attachment_file(path)?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let Reply::ResourceCreated(resource) = call(
        client,
        Rpc::CreateUpload {
            account,
            name,
            media_type: "application/octet-stream".into(),
            size,
        },
    )?
    else {
        return Err("附件上传响应无效".into());
    };
    let result = (|| {
        let mut buffer = vec![0; mail_protocol::MAX_CHUNK_BYTES as usize];
        let mut offset = 0;
        while offset < size {
            let remaining = (size - offset).min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..remaining])
                .map_err(|e| e.to_string())?;
            client
                .write_chunk(&resource.id, offset, &buffer[..remaining])
                .map_err(|e| e.to_string())?;
            offset += remaining as u64;
        }
        let mut extra = [0];
        if file.read(&mut extra).map_err(|e| e.to_string())? != 0 {
            return Err("上传期间附件发生变化，请重新选择".into());
        }
        match call(
            client,
            Rpc::FinishUpload {
                resource: resource.id.clone(),
            },
        )? {
            Reply::Attachment(info) => Ok(info),
            _ => Err("附件导入结果未确认，请检查后重新选择".into()),
        }
    })();
    let _ = call(
        client,
        Rpc::CancelResource {
            resource: resource.id,
        },
    );
    result
}
