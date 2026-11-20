//! Opening a displayed message persists a single read flag through the normal
//! offline queue. Selection/check boxes and SMTP are deliberately independent.
use super::*;
use mail_protocol::{Command, MailOperation, MutationStatus, Request};

#[derive(Default)]
pub(super) struct ReadMarks {
    pending: HashSet<mail_core::MailUid>,
    pub manual_revision: u64,
}

fn read_request(location: mail_core::MailUid) -> Result<Request, String> {
    Ok(Request::new(
        mail_protocol::transport::request_id().map_err(|e| e.to_string())?,
        Command::EnqueueOperation {
            locations: vec![location],
            operation: MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
        },
    ))
}

fn acknowledge(reply: Reply, location: &mail_core::MailUid) -> Result<(), String> {
    match reply {
        Reply::Operations(records)
            if records.len() == 1
                && records[0].location == *location
                && records[0].operation
                    == MailOperation::Flags {
                        seen: Some(true),
                        flagged: None,
                    } =>
        {
            Ok(())
        }
        _ => Err("自动已读响应未确认，请查看同步状态".into()),
    }
}

fn recover_acknowledgement(
    result: Result<Reply, String>,
    request: &Request,
    location: &mail_core::MailUid,
    mut lookup: impl FnMut(Command) -> Result<Reply, String>,
) -> Result<(), String> {
    match result {
        Ok(reply) => acknowledge(reply, location),
        Err(error) => match lookup(Command::MutationResult {
            mutation: request.id.clone(),
        }) {
            Ok(Reply::Mutation(MutationStatus::Finished(result))) => {
                acknowledge((*result).map_err(|e| e.message)?, location)
            }
            _ => Err(error),
        },
    }
}

fn mark_projection(messages: &mut [CachedMessage], location: &mail_core::MailUid) {
    for message in messages {
        if message.location == *location {
            message.seen = true;
        }
    }
}

impl Mailbox {
    pub(super) fn mark_visible_mail_read(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self
            .inline
            .as_ref()
            .and_then(|inline| inline.location.clone())
        else {
            return;
        };
        if self.selected_location() != Some(&location)
            || self.read_marks.pending.contains(&location)
        {
            return;
        }
        let unread = if let Some(hits) = &self.search_results {
            hits.iter()
                .any(|hit| hit.message.location == location && !hit.message.seen)
        } else {
            self.messages()
                .iter()
                .any(|message| message.location == location && !message.seen)
        };
        if !unread {
            return;
        }
        let request = match read_request(location.clone()) {
            Ok(request) => request,
            Err(error) => {
                self.status = error;
                cx.notify();
                return;
            }
        };
        // Submit on the UI thread through a nonblocking queue so a later explicit
        // "mark unread" is ordered after this intent. Never include checked mail.
        let reply = match self.store.rpc(request.clone()) {
            Ok(reply) => reply,
            Err(error) => {
                self.status = format!("邮件已打开，但自动已读未保存：{error}");
                cx.notify();
                return;
            }
        };
        self.read_marks.pending.insert(location.clone());
        let manual_revision = self.read_marks.manual_revision;
        let store = self.store.clone();
        let target = location.clone();
        let task = cx.background_executor().spawn(async move {
            let result = reply
                .recv_timeout(std::time::Duration::from_secs(6))
                .map_err(|_| "自动已读保存未确认".to_owned())
                .and_then(|r| r)
                .and_then(|response| response.result.map_err(|e| e.message));
            // A lost acknowledgement queries the original persistent request;
            // it must not enqueue another operation with a new identity.
            recover_acknowledgement(result, &request, &target, |command| store.control(command))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.read_marks.pending.remove(&location);
                match result {
                    Ok(()) if this.read_marks.manual_revision == manual_revision => {
                        if let Some(cache) = &mut this.cache {
                            mark_projection(&mut cache.messages, &location);
                        }
                        if let Some(inbox) = &mut this.inbox {
                            mark_projection(&mut inbox.messages, &location);
                        }
                        if let Some(hits) = &mut this.search_results {
                            for hit in hits {
                                mark_projection(std::slice::from_mut(&mut hit.message), &location);
                            }
                        }
                    }
                    Ok(()) => (), // A later explicit flag action owns the display.
                    Err(error) => {
                        this.status = format!("邮件已打开，但自动已读保存未确认：{error}")
                    }
                }
                this.refresh_sync_status(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_protocol::{OperationInfo, OperationState};
    fn location() -> mail_core::MailUid {
        mail_core::MailUid {
            account: AccountId("a".repeat(32)),
            folder_id: "f".repeat(32),
            uidvalidity: 3,
            uid: 7,
        }
    }
    fn reply(location: mail_core::MailUid) -> Reply {
        Reply::Operations(vec![OperationInfo {
            id: "b".repeat(32),
            location,
            operation: MailOperation::Flags {
                seen: Some(true),
                flagged: None,
            },
            state: OperationState::Pending,
            detail: None,
        }])
    }
    #[test]
    fn auto_read_acknowledgement_recovery_queries_original_request_without_replay() {
        let location = location();
        let request = read_request(location.clone()).unwrap();
        let mut commands = vec![];
        recover_acknowledgement(
            Err("lost response".into()),
            &request,
            &location,
            |command| {
                commands.push(command);
                Ok(Reply::Mutation(MutationStatus::Finished(Box::new(Ok(
                    reply(location.clone()),
                )))))
            },
        )
        .unwrap();
        assert!(
            matches!(&commands[..], [Command::MutationResult { mutation }] if mutation == &request.id)
        );
        let mut other = location.clone();
        other.account = AccountId("c".repeat(32));
        assert!(acknowledge(reply(other), &location).is_err());
        assert!(
            recover_acknowledgement(Err("lost response".into()), &request, &location, |_| Ok(
                Reply::Mutation(MutationStatus::Uncertain)
            ))
            .is_err()
        );
    }
    #[test]
    fn auto_read_projection_preserves_same_uid_in_other_accounts_folders_and_epochs() {
        let target = location();
        let mut locations = vec![target.clone(); 4];
        locations[1].account = AccountId("c".repeat(32));
        locations[2].folder_id = "d".repeat(32);
        locations[3].uidvalidity += 1;
        let mut messages = locations
            .into_iter()
            .enumerate()
            .map(|(i, location)| CachedMessage {
                id: mail_core::MessageId(i.to_string()),
                location,
                envelope: mail_core::MessageEnvelope {
                    subject: "same".into(),
                    sender: "same".into(),
                    received_at: 1,
                },
                seen: false,
                flagged: true,
            })
            .collect::<Vec<_>>();
        mark_projection(&mut messages, &target);
        assert_eq!(
            messages.iter().map(|m| m.seen).collect::<Vec<_>>(),
            [true, false, false, false]
        );
        assert!(messages.iter().all(|m| m.flagged));
    }
}
