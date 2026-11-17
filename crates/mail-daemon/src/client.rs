//! 前台业务适配；仅调用IPC，不取凭据、不访问SQLite、不直接访问邮件服务器。
use mail_protocol::{
    transport::{Client, request_id},
    *,
};
use std::time::{Duration, Instant};
pub fn run(client: &Client, operation: BusinessOperation) -> Result<Reply, String> {
    let writes = operation.writes();
    let request = Request::new(
        request_id().map_err(|e| e.to_string())?,
        Command::Business { operation },
    );
    match client.call(&request) {
        Ok(Reply::Pending { .. }) => (),
        Ok(reply) => return Ok(reply),
        Err(transport::ClientError::Remote(error)) => return Err(error.message),
        Err(error) if !writes => return Err(error.to_string()),
        Err(_) => (),
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        match client.request(Command::JobResult {
            task: request.id.clone(),
        }) {
            Ok(Reply::Job {
                result: Some(result),
            }) => return (*result).map_err(|e| e.message),
            Ok(Reply::Job { result: None }) => std::thread::sleep(Duration::from_millis(150)),
            _ if writes => {
                return match client.request(Command::MutationResult {
                    mutation: request.id.clone(),
                }) {
                    Ok(Reply::Mutation(MutationStatus::Finished(result))) => {
                        (*result).map_err(|e| e.message)
                    }
                    _ => Err("在线操作结果未确认，请同步核对；不会自动重放".into()),
                };
            }
            _ => return Err("后台已断开或任务失效，请重连后重新读取".into()),
        }
    }
    Err("后台任务超时；若涉及在线修改，请先同步核对，不要重复执行".into())
}
pub fn content(
    client: &Client,
    operation: BusinessOperation,
) -> Result<mail_content::MailContent, String> {
    let Reply::Message(view) = run(client, operation)? else {
        return Err("阅读响应类型无效".into());
    };
    materialize(client, view)
}
fn materialize(client: &Client, view: MessageView) -> Result<mail_content::MailContent, String> {
    let refs = std::iter::once(&view.document)
        .chain(std::iter::once(&view.plain))
        .chain(view.images.iter().map(|i| &i.resource))
        .chain(view.attachments.iter().map(|a| &a.resource))
        .cloned()
        .collect::<Vec<_>>();
    let result = (|| {
        if refs
            .iter()
            .try_fold(0u64, |size, r| size.checked_add(r.size))
            .is_none_or(|size| size > 32 * 1024 * 1024)
        {
            return Err("阅读资源超出传输上限".into());
        }
        let mut content = mail_content::parse(b"").map_err(|e| e.to_string())?;
        content.display_handle = Some(view.document.id.clone());
        content.document = serde_json::from_slice(
            &client
                .read_resource(&view.document, 4 * 1024 * 1024)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("阅读树无效：{e}"))?;
        content.plain = String::from_utf8(
            client
                .read_resource(&view.plain, mail_content::MAX_BODY_BYTES as u64)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|_| "正文编码无效")?;
        content.subject = view.subject;
        content.sender = view.sender;
        content.from = view.from;
        content.to = view.to;
        content.cc = view.cc;
        content.reply_to = view.reply_to;
        content.date_unix = view.date_unix;
        content.date_text = view.date_text;
        content.warnings = view.warnings;
        content.is_html = view.is_html;
        content.remote_images = vec![String::new(); view.remote_images.min(1024) as usize];
        for image in view.images {
            let bytes = client
                .read_resource(&image.resource, mail_content::MAX_IMAGE_BYTES as u64)
                .map_err(|e| e.to_string())?;
            content.images.push(
                mail_content::validate_image(&bytes, "image/png", image.key)
                    .map_err(|e| e.to_string())?,
            );
        }
        for attachment in view.attachments {
            let bytes = client
                .read_resource(&attachment.resource, mail_content::MAX_MESSAGE_BYTES as u64)
                .map_err(|e| e.to_string())?;
            content.attachments.push(mail_content::Attachment {
                name: attachment.name,
                media_type: attachment.resource.media_type,
                content_id: None,
                bytes: bytes.into(),
            });
        }
        Ok(content)
    })();
    for reference in refs {
        let _ = client.request(Command::CancelResource {
            resource: reference.id,
        });
    }
    result
}
