//! 明确发送确认；预览不联网、不排队，发送结果持续展示。
use super::*;
use mail_core::SendState;
fn state_label(state: SendState) -> &'static str {
    match state {
        SendState::Pending => "待提交；不会自动发送",
        SendState::Sending => "正在交给邮件服务器；请勿重复发送",
        SendState::Sent => "发送成功，已保存副本",
        SendState::Failed => "发送未完成；不会自动重试",
        SendState::Uncertain => "结果不确定：收件人可能已收到，请勿重复发送",
        SendState::CopyPending => "邮件服务器已接受；已发送副本尚未保存，请勿重发",
    }
}
impl Composer {
    pub(super) fn send_locked(&self) -> bool {
        self.checking_send || self.sending || self.send_review.is_some()
    }
    pub(super) fn refresh_send_tasks(&mut self, cx: &mut Context<Self>) {
        self.send_tasks_generation += 1;
        let generation = self.send_tasks_generation;
        self.send_tasks.clear();
        let Some(draft) = self.stored.as_ref() else {
            return;
        };
        let id = draft.id.clone();
        let request = crate::lifecycle::storage(cx).draft_tasks(draft.account.clone(), id.clone());
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                if this.send_tasks_generation == generation
                    && this.stored.as_ref().is_some_and(|d| d.id == id)
                {
                    match result {
                        Ok(tasks) => this.send_tasks = tasks,
                        Err(e) => this.status = format!("发送记录读取失败，请勿重复发送：{e}"),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn check_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.closing || self.importing || self.send_locked() {
            return;
        }
        if self.account_scope.is_none() {
            self.status = "请从邮箱账号进入写信；未关联的本地草稿不能发送".into();
            cx.notify();
            return;
        }
        if self.composing(window, cx) {
            self.status = "请先完成或取消中文候选词输入".into();
            cx.notify();
            return;
        }
        if self.saving || self.saved != Some(self.session.borrow().revision) {
            self.save(cx);
            self.status = "正在保存草稿，请等已保存后再检查发送".into();
            cx.notify();
            return;
        }
        let Some(draft) = self.stored.clone() else {
            return;
        };
        let revision = self.session.borrow().revision;
        self.checking_send = true;
        self.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        self.status = "正在检查已保存的收件人、正文与附件…".into();
        let request = crate::lifecycle::storage(cx).review_send(draft);
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.checking_send = false;
                if this.closing {
                    return;
                }
                match result {
                    Ok(review)
                        if this.saved == Some(revision)
                            && this.session.borrow().revision == revision =>
                    {
                        this.send_review = Some((review, revision));
                        this.status = "请核对发送内容；只有点击确认才会提交".into();
                    }
                    Ok(_) => this.status = "草稿已修改，请重新检查发送内容".into(),
                    Err(e) => {
                        this.status = format!("未发送：{e}");
                        this.refresh_send_tasks(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn confirm_send(&mut self, cx: &mut Context<Self>) {
        if self.closing || self.sending {
            return;
        }
        let Some((review, revision)) = self.send_review.take() else {
            return;
        };
        if self.saved != Some(revision) || self.session.borrow().revision != revision {
            self.status = "内容已变化，请重新检查发送".into();
            cx.notify();
            return;
        }
        let empty = review.headers.subject.trim().is_empty();
        let result =
            crate::lifecycle::sending(cx).confirm(crate::lifecycle::storage(cx), review, empty);
        let reply = match result {
            Ok(reply) => reply,
            Err(e) => {
                self.status = e;
                cx.notify();
                return;
            }
        };
        self.sending = true;
        self.status = "正在提交；关闭写信窗口不会取消，结果将可靠保存在发送记录".into();
        let task = cx.background_executor().spawn(async move {
            reply
                .recv()
                .map_err(|_| "发送任务中断，请查看记录，勿重复发送".to_owned())
                .and_then(|r| r)
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.sending = false;
                this.status = match result {
                    Ok(task) => state_label(task.state).into(),
                    Err(e) => format!("未确认发送完成：{e}"),
                };
                this.refresh_send_tasks(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn cancel_review(&mut self, cx: &mut Context<Self>) {
        self.send_review = None;
        self.status = "已取消发送确认，草稿保留".into();
        cx.notify();
    }
    pub(super) fn send_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut area = div().flex().flex_col().gap_1();
        let mut actions = div().flex().gap_2().flex_wrap().child(
            MaterialButton::new("check-send")
                .child("检查并发送…")
                .disabled(
                    self.loading
                        || self.closing
                        || self.importing
                        || self.send_locked()
                        || self.account_scope.is_none()
                        || !self.send_tasks.is_empty(),
                )
                .on_click(cx.listener(|this, _, window, cx| this.check_send(window, cx))),
        );
        actions = actions.child(
            MaterialButton::new("refresh-send-tasks")
                .child("刷新发送记录")
                .disabled(self.loading)
                .on_click(cx.listener(|this, _, _, cx| this.refresh_send_tasks(cx))),
        );
        area = area.child(actions);
        if let Some((review, _)) = &self.send_review {
            let empty = review.headers.subject.trim().is_empty();
            let summary = format!(
                "发件人：{}\n收件人：{}\n抄送：{}\n密送：{}\n主题：{}\n附件：{}个，共{}字节\n正文摘要：{}",
                review.sender,
                review.headers.to.join(", "),
                review.headers.cc.join(", "),
                review.headers.bcc.join(", "),
                if empty {
                    "（无主题）"
                } else {
                    &review.headers.subject
                },
                review.attachments,
                review.attachment_bytes,
                review.preview
            );
            area = area
                .child(
                    div()
                        .id("send-review")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .p_2()
                        .child(summary),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            MaterialButton::new("confirm-send")
                                .child(if empty {
                                    "确认无主题仍发送"
                                } else {
                                    "确认发送"
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_send(cx))),
                        )
                        .child(
                            MaterialButton::new("cancel-send-review")
                                .child("返回编辑")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_review(cx);
                                })),
                        ),
                );
        }
        let mut records = div()
            .id("send-records")
            .max_h(px(90.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        for task in &self.send_tasks {
            records = records.child(div().text_size(px(13.)).child(format!(
                "任务 {} · 草稿版本{} · {}",
                &task.id.0[..8],
                task.source_revision,
                state_label(task.state)
            )));
        }
        area.child(records)
    }
    pub(super) fn send_probe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.probe_step {
            0 => {
                assert!(self.account_scope.is_some());
                self.header_inputs[0].update(cx, |input, cx| {
                    input.replace_text_in_range(None, "to@example.invalid", window, cx)
                });
                self.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "发送预览正文🙂", window, cx)
                });
                self.probe_step = 1;
            }
            1 => {
                if self.saving {
                    return;
                }
                if self.saved != Some(self.session.borrow().revision) {
                    self.save(cx);
                    return;
                }
                self.check_send(window, cx);
                self.probe_step = 2;
            }
            2 => {
                if self.checking_send {
                    return;
                }
                let (review, _) = self
                    .send_review
                    .as_ref()
                    .expect("empty subject must reach review");
                assert!(review.headers.subject.is_empty());
                assert_eq!(review.headers.to, ["to@example.invalid"]);
                assert!(review.preview.contains("发送预览正文🙂"));
                assert!(self.send_locked());
                assert!(self.send_tasks.is_empty());
                // 复用实际取消动作，下一次render解除字段只读后再模拟输入。
                self.cancel_review(cx);
                self.probe_step = 3;
            }
            3 => {
                self.header_inputs[0].update(cx, |input, cx| {
                    let end = input.value().encode_utf16().count();
                    input.replace_text_in_range(Some(0..end), "partial@", window, cx);
                    assert_eq!(input.value().as_ref(), "partial@");
                });
                self.probe_step = 4;
            }
            4 => {
                if self.saving {
                    return;
                }
                if self.saved != Some(self.session.borrow().revision) {
                    self.save(cx);
                    return;
                }
                self.check_send(window, cx);
                self.probe_step = 5;
            }
            5 => {
                if self.checking_send {
                    return;
                }
                assert!(self.send_review.is_none());
                assert!(self.status.contains("收件人"), "{}", self.status);
                assert!(self.send_tasks.is_empty());
                assert!(!crate::lifecycle::sending(cx).active());
                eprintln!(
                    "QINGMAIL_SEND_REVIEW_PROBE: PASS · account draft / empty subject review / cancel / invalid recipient / no send"
                );
                cx.quit();
                self.probe_step = 6;
            }
            _ => {}
        }
    }
}
