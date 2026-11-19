//! 列表按需追加；继续使用后台的有界查询，不改变存储或 IPC。
use super::*;

pub(super) fn append_unique(messages: &mut Vec<CachedMessage>, batch: Vec<CachedMessage>) {
    let mut seen: HashSet<_> = messages.iter().map(|m| m.location.clone()).collect();
    messages.extend(
        batch
            .into_iter()
            .filter(|m| seen.insert(m.location.clone())),
    );
}

pub(super) fn folder_window(
    store: &StorageClient,
    account: AccountId,
    name: String,
    before: Option<u32>,
    target: usize,
) -> Result<(Option<FolderCache>, bool), String> {
    let Some(mut cache) = receive(store.folder_cache(account.clone(), name.clone(), before))?
    else {
        return Ok((None, true));
    };
    let mut exhausted = cache.messages.len() < 200;
    while !exhausted && cache.messages.len() < target {
        let cursor = cache.messages.last().map(|m| m.location.uid);
        let next = receive(store.folder_cache(account.clone(), name.clone(), cursor))?
            .ok_or("文件夹缓存已变化，请刷新")?;
        if cache.folder != next.folder {
            return Err("文件夹身份已变化，请刷新".into());
        }
        exhausted = next.messages.len() < 200;
        let previous = cache.messages.len();
        append_unique(&mut cache.messages, next.messages);
        if cache.messages.len() == previous {
            exhausted = true;
        }
    }
    Ok((Some(cache), exhausted))
}

pub(super) fn inbox_window(
    store: &StorageClient,
    before: Option<mail_protocol::InboxCursor>,
    target: usize,
) -> Result<mail_protocol::MessagePage, String> {
    collect_inbox(before, target, |before| {
        match store.control(mail_protocol::Command::UnifiedInbox { before, limit: 100 })? {
            Reply::Inbox(page) => Ok(page),
            _ => Err("统一收件箱响应无效".into()),
        }
    })
}

fn collect_inbox(
    mut before: Option<mail_protocol::InboxCursor>,
    target: usize,
    mut fetch: impl FnMut(
        Option<mail_protocol::InboxCursor>,
    ) -> Result<mail_protocol::MessagePage, String>,
) -> Result<mail_protocol::MessagePage, String> {
    let mut messages = Vec::new();
    loop {
        let page = fetch(before.clone())?;
        let previous = messages.len();
        append_unique(&mut messages, page.messages);
        let next = page.next;
        if next.is_some() && (next == before || messages.len() == previous) {
            return Err("列表游标未前进，请刷新后重试".into());
        }
        if next.is_none() || messages.len() >= target {
            return Ok(mail_protocol::MessagePage { messages, next });
        }
        before = next;
    }
}

enum Batch {
    Inbox(mail_protocol::MessagePage),
    Folder(Option<FolderCache>, bool),
}

impl Mailbox {
    pub(super) fn can_load_more(&self) -> bool {
        !self.busy
            && !self.list_exhausted
            && !self.list_load_failed
            && self.search_results.is_none()
            && !self.messages().is_empty()
    }

    pub(super) fn load_more(&mut self, cx: &mut Context<Self>) {
        if !self.can_load_more() {
            return;
        }
        let store = self.store.clone();
        let unified = self.unified;
        let cursor = self.inbox.as_ref().and_then(|p| p.next.clone());
        let account = self.account.as_ref().map(|(id, _)| id.clone());
        let name = self.selected_folder.clone();
        let before = self.messages().last().map(|m| m.location.uid);
        self.busy = true;
        let task = cx.background_executor().spawn(async move {
            if unified {
                if cursor.is_none() {
                    return Err("列表已无更多邮件".into());
                }
                Ok(Batch::Inbox(inbox_window(&store, cursor, 100)?))
            } else {
                let (cache, exhausted) = folder_window(
                    &store,
                    account.ok_or("未选择账号")?,
                    name.ok_or("未选择文件夹")?,
                    before,
                    200,
                )?;
                Ok::<_, String>(Batch::Folder(cache, exhausted))
            }
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                let result = match result {
                    Ok(Batch::Inbox(page)) => {
                        if let Some(inbox) = &mut this.inbox {
                            this.list_exhausted = page.next.is_none();
                            inbox.next = page.next;
                            append_unique(&mut inbox.messages, page.messages);
                        }
                        Ok(())
                    }
                    Ok(Batch::Folder(Some(batch), exhausted)) => {
                        if let Some(cache) = &mut this.cache {
                            if cache.folder == batch.folder {
                                this.list_exhausted = exhausted;
                                append_unique(&mut cache.messages, batch.messages);
                                Ok(())
                            } else {
                                Err("文件夹身份已变化，请刷新".to_string())
                            }
                        } else {
                            Err("文件夹缓存已变化，请刷新".to_string())
                        }
                    }
                    Ok(Batch::Folder(None, _)) => Err("文件夹缓存已变化，请刷新".to_string()),
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    this.list_load_failed = true;
                    this.status = format!("加载更多失败：{error}，已保留列表");
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(account: &str, epoch: u32, uid: u32) -> CachedMessage {
        CachedMessage {
            id: mail_core::MessageId(format!("{account}-{epoch}-{uid}")),
            location: mail_core::MailUid {
                account: AccountId(account.into()),
                folder_id: "inbox".into(),
                uidvalidity: epoch,
                uid,
            },
            envelope: mail_core::MessageEnvelope {
                subject: String::new(),
                sender: String::new(),
                received_at: 0,
            },
            seen: false,
            flagged: false,
        }
    }
    #[test]
    fn append_keeps_loaded_order_and_full_account_identity() {
        let mut loaded = vec![message("a", 1, 3), message("a", 1, 2)];
        append_unique(
            &mut loaded,
            vec![
                message("a", 1, 2),
                message("a", 1, 1),
                message("b", 1, 1),
                message("a", 2, 1),
            ],
        );
        assert_eq!(loaded.len(), 5);
        assert_eq!(loaded[0].location.uid, 3);
        assert_eq!(loaded[2].location.uid, 1);
        assert_eq!(loaded[3].location.account.0, "b");
        assert_eq!(loaded[4].location.uidvalidity, 2);
        append_unique(&mut loaded, vec![]);
        assert_eq!(loaded.len(), 5);
    }
    #[test]
    fn folder_scroll_and_refresh_span_multiple_storage_batches() {
        let root = std::env::temp_dir().join(format!(
            "qingmail-list-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let mut store = mail_storage::Store::open(&root).unwrap();
        let server = mail_core::ServerSettings {
            authentication: Default::default(),
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: mail_core::ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "synthetic".into(),
                address: "synthetic@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let batch = |min: u32, max: u32| mail_core::FolderBatch {
            remote_name: "INBOX".into(),
            uidvalidity: 42,
            since: min as i64,
            has_more: false,
            complete: true,
            kind: mail_core::BatchKind::Recent,
            flags: Default::default(),
            messages: (min..=max)
                .map(|uid| {
                    (
                        uid,
                        mail_core::MessageEnvelope {
                            subject: uid.to_string(),
                            sender: "synthetic".into(),
                            received_at: uid as i64,
                        },
                    )
                })
                .collect(),
        };
        drop(store);
        let mut worker = mail_daemon::storage::StorageWorker::start(Some(root)).unwrap();
        let client = worker.client();
        receive(client.publish_catalog(
            account.clone(),
            vec![RemoteFolder {
                name: "INBOX".into(),
                display_name: "INBOX".into(),
                delimiter: None,
                selectable: true,
            }],
            1,
        ))
        .unwrap();
        for (min, max) in [(1, 200), (201, 400), (401, 450)] {
            receive(client.publish_folder(account.clone(), batch(min, max), 1)).unwrap();
        }
        let (first, exhausted) =
            folder_window(&client, account.clone(), "INBOX".into(), None, 200).unwrap();
        assert!(!exhausted);
        let mut loaded = first.unwrap().messages;
        assert_eq!(loaded.len(), 200);
        let (next, exhausted) =
            folder_window(&client, account.clone(), "INBOX".into(), Some(251), 200).unwrap();
        assert!(!exhausted);
        append_unique(&mut loaded, next.unwrap().messages);
        let (last, exhausted) =
            folder_window(&client, account.clone(), "INBOX".into(), Some(51), 200).unwrap();
        assert!(exhausted);
        append_unique(&mut loaded, last.unwrap().messages);
        assert_eq!(loaded.len(), 450);
        assert_eq!(loaded.first().unwrap().location.uid, 450);
        assert_eq!(loaded.last().unwrap().location.uid, 1);
        // 刷新重新查询已加载深度，既不缩回首批，也不把服务器删除的邮件拼回来。
        receive(client.publish_folder(account.clone(), batch(401, 449), 2)).unwrap();
        let (refreshed, exhausted) =
            folder_window(&client, account, "INBOX".into(), None, loaded.len()).unwrap();
        let refreshed = refreshed.unwrap();
        assert!(exhausted);
        assert_eq!(refreshed.messages.len(), 449);
        assert_eq!(refreshed.messages[0].location.uid, 449);
        worker.stop();
    }
    #[test]
    fn unified_cursor_loads_across_accounts_and_stops_at_end() {
        let cursor = mail_protocol::InboxCursor {
            received_at: 1,
            account: AccountId("a".into()),
            message: mail_core::MessageId("cursor".into()),
        };
        let mut requests = 0;
        let page = collect_inbox(None, 200, |before| {
            requests += 1;
            match requests {
                1 => {
                    assert!(before.is_none());
                    Ok(mail_protocol::MessagePage {
                        messages: vec![message("a", 1, 1)],
                        next: Some(cursor.clone()),
                    })
                }
                2 => {
                    assert_eq!(before, Some(cursor.clone()));
                    Ok(mail_protocol::MessagePage {
                        messages: vec![message("b", 1, 1)],
                        next: None,
                    })
                }
                _ => panic!("end of inbox must stop further requests"),
            }
        })
        .unwrap();
        assert_eq!(requests, 2);
        assert_eq!(page.messages.len(), 2);
        assert!(page.next.is_none());
        // 不前进的游标必须失败，避免渲染不断触发相同请求。
        assert!(
            collect_inbox(Some(cursor.clone()), 100, |_| Ok(
                mail_protocol::MessagePage {
                    messages: vec![message("a", 1, 1)],
                    next: Some(cursor.clone()),
                }
            ))
            .is_err()
        );
    }
}
