//! 写信字段与附件；和正文共享版本及关闭保存流程，不创建发送任务。
use super::*;
use gpui_kit::{Focusable, PathPromptOptions, base::input::InputEvent};

pub(super) fn headers(values: &[String; 4]) -> mail_core::DraftHeaders {
    let addresses = |value: &str| {
        value
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    mail_core::DraftHeaders {
        to: addresses(&values[0]),
        cc: addresses(&values[1]),
        bcc: addresses(&values[2]),
        subject: values[3].clone(),
    }
}
impl Composer {
    pub(super) fn subscribe_headers(&mut self, cx: &mut Context<Self>) {
        self.header_subscriptions = self
            .header_inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                cx.subscribe(input, move |this, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Focus | InputEvent::Blur) {
                        cx.notify();
                    }
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        if this.header_values[index] != value {
                            this.header_values[index] = value;
                            this.field_errors[index] = None;
                            this.session.borrow_mut().revision += 1;
                            this.last_edit = Instant::now();
                            cx.notify();
                        }
                    }
                })
            })
            .collect();
    }
    pub(super) fn restore_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.stored.as_ref() else {
            return;
        };
        let h = &draft.document.headers;
        self.header_values = [
            h.to.join(", "),
            h.cc.join(", "),
            h.bcc.join(", "),
            h.subject.clone(),
        ];
        self.attachment_ids = draft.document.attachments.clone();
        if !h.cc.is_empty() || !h.bcc.is_empty() {
            self.show_cc_bcc = true;
        }
        self.attachment_infos.clear();
        let id = draft.id.clone();
        let request = crate::lifecycle::storage(cx)
            .attachment_infos(draft.account.clone(), self.attachment_ids.clone());
        for (input, value) in self.header_inputs.iter().zip(&self.header_values) {
            input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                if this.stored.as_ref().is_some_and(|d| d.id == id) {
                    match result {
                        Ok(infos) => {
                            // 导入可能已完成；合并而不覆盖较新的附件元数据。
                            for info in infos {
                                if this.attachment_ids.contains(&info.id)
                                    && !this.attachment_infos.iter().any(|i| i.id == info.id)
                                {
                                    this.attachment_infos.push(info);
                                }
                            }
                        }
                        Err(e) => this.status = format!("附件信息读取失败：{e}"),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn add_attachment(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.closing || self.importing || self.send_locked() {
            return;
        }
        let Some(draft) = self.stored.as_ref() else {
            return;
        };
        if self.attachment_ids.len() >= 32 {
            self.status = "最多32个附件，请先移除部分附件".into();
            cx.notify();
            return;
        }
        let account = draft.account.clone();
        let draft_id = draft.id.clone();
        self.importing = true;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("添加附件（复制到草稿，不修改原文件）".into()),
        });
        let store = crate::lifecycle::storage(cx);
        cx.spawn(async move |entity, cx| {
            let result = match picker.await {
                Ok(Ok(Some(paths))) if paths.len() == 1 => {
                    let path = paths.into_iter().next().unwrap();
                    cx.background_executor().spawn(async move {receive(store.import_attachment(account, path)).map(Some)}).await
                }
                Ok(Ok(_)) => Ok(None),
                _ => Err("文件选择器不可用或未响应".into()),
            };
            let _ = entity.update(cx, |this, cx| {
                this.importing = false;
                if !this.stored.as_ref().is_some_and(|d| d.id == draft_id) { return; }
                match result {
                    Ok(Some(info)) => this.accept_attachment(info, cx),
                    Ok(None) => this.status = "已取消添加附件".into(),
                    Err(e) => this.status = format!("添加附件失败，原草稿保留：{e}"),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn accept_attachment(
        &mut self,
        info: mail_storage::AttachmentInfo,
        cx: &mut Context<Self>,
    ) {
        let total: u64 = self
            .attachment_infos
            .iter()
            .filter(|i| self.attachment_ids.contains(&i.id))
            .map(|i| u64::from(i.size))
            .sum();
        if self.attachment_ids.len() >= 32
            || self.attachment_infos.len() < self.attachment_ids.len()
            || total + u64::from(info.size) > mail_storage::MAX_ATTACHMENT_BYTES
        {
            self.status = "附件数量或总量超限，或原附件信息未确认，未加入草稿".into();
        } else {
            self.attachment_ids.push(info.id.clone());
            self.attachment_infos.push(info);
            self.session.borrow_mut().revision += 1;
            self.last_edit = Instant::now();
            // 让tick确认所有字段和正文的IME组合结束后再保存。
            self.status = "附件已复制，等待自动保存草稿".into();
        }
        cx.notify();
    }
    /// 发送前在本地定位字段错误；不依赖存储层的字符串错误。
    pub(super) fn validate_fields(&mut self, cx: &App) -> bool {
        let mut errors = [None, None, None, None];
        let mut total = 0usize;
        for index in 0..3 {
            let value = self.header_inputs[index].read(cx).value().to_string();
            let mut position = 0;
            for address in value.split([',', ';']).map(str::trim) {
                if address.is_empty() {
                    continue;
                }
                total += 1;
                position += 1;
                if !valid_address(address) {
                    errors[index] = Some(format!("第 {position} 个地址格式不正确"));
                    break;
                }
            }
        }
        if total == 0 {
            errors[0] = Some("至少填写一个收件人、抄送或密送地址".into());
        }
        let subject = self.header_inputs[3].read(cx).value().to_string();
        if subject.len() > 4096 || subject.chars().any(char::is_control) {
            errors[3] = Some("主题含控制字符或超过上限".into());
        }
        self.field_errors = errors;
        self.field_errors.iter().all(Option::is_none)
    }
    fn recipient_chips(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_wrap().gap_1();
        let value = self.header_inputs[index].read(cx).value().to_string();
        for address in value
            .split([',', ';'])
            .map(str::trim)
            .filter(|address| !address.is_empty())
        {
            let address = address.to_owned();
            let removed = address.clone();
            row = row.child(
                MaterialButton::new(format!("chip-{index}-{address}"))
                    .variant(ButtonVariant::Tonal)
                    .button_size(ButtonSize::ExtraSmall)
                    .child(format!("{address} ×"))
                    .disabled(self.loading || self.closing || self.send_locked())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_recipient(index, &removed, window, cx);
                    })),
            );
        }
        row
    }
    fn remove_recipient(
        &mut self,
        index: usize,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remaining = self.header_inputs[index]
            .read(cx)
            .value()
            .split([',', ';'])
            .map(str::trim)
            .filter(|address| !address.is_empty() && *address != value)
            .collect::<Vec<_>>()
            .join(", ");
        self.header_inputs[index].update(cx, |input, cx| {
            input.set_value(remaining, window, cx);
        });
        cx.notify();
    }
    pub(super) fn fields_toolbar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let disabled = self.loading || self.closing || self.store.is_none() || self.send_locked();
        let mut fields = div()
            .flex()
            .flex_col()
            .flex_none()
            .max_h(px(240.))
            .id("compose-headers")
            .overflow_y_scroll();
        for (index, label) in ["收件人", "抄送", "密送", "主题"].into_iter().enumerate() {
            if (index == 1 || index == 2) && !self.show_cc_bcc {
                continue;
            }
            let editing = index == 3
                || self.header_values[index].is_empty()
                || self.header_inputs[index]
                    .focus_handle(cx)
                    .is_focused(window);
            fields =
                fields.child(
                    div()
                        .px_4()
                        .py_1()
                        .border_b_1()
                        .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().w(px(54.)).flex_none().text_sm().child(label))
                                .when(editing, |row| {
                                    row.child(
                                        TextField::new(
                                            ("draft-header", index),
                                            &self.header_inputs[index],
                                        )
                                        .density(TextFieldDensity::Compact)
                                        .placeholder(if index == 3 {
                                            "填写主题"
                                        } else {
                                            "输入邮箱地址，以逗号分隔"
                                        })
                                        .disabled(disabled)
                                        .flex_1()
                                        .min_w_0(),
                                    )
                                })
                                .when(!editing, |row| {
                                    row.child(
                                        div()
                                            .min_h(px(48.))
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(self.recipient_chips(index, cx))
                                            .child(
                                                crate::ui::tool(
                                                    ("edit-recipients", index),
                                                    "edit",
                                                    "编辑收件地址",
                                                )
                                                .button_size(ButtonSize::ExtraSmall)
                                                .disabled(disabled)
                                                .on_click(cx.listener(move |this, _, w, cx| {
                                                    this.header_inputs[index]
                                                        .update(cx, |i, cx| i.focus(w, cx))
                                                })),
                                            ),
                                    )
                                })
                                .when(index == 0, |row| {
                                    row.child(
                                        MaterialButton::new("toggle-cc-bcc")
                                            .variant(ButtonVariant::Text)
                                            .button_size(ButtonSize::ExtraSmall)
                                            .child(if self.show_cc_bcc {
                                                "收起"
                                            } else {
                                                "抄送 / 密送"
                                            })
                                            .disabled(disabled)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_cc_bcc = !this.show_cc_bcc;
                                                cx.notify();
                                            })),
                                    )
                                }),
                        )
                        .when_some(self.field_errors[index].clone(), |row, error| {
                            row.child(
                                div()
                                    .text_sm()
                                    .text_color(theme::color(ColorRole::ERROR))
                                    .child(error),
                            )
                        }),
                );
        }
        fields
    }
    pub(super) fn attachment_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.loading || self.closing || self.store.is_none() || self.send_locked();
        let mut row = div().flex().flex_wrap().gap_2();
        for (index, id) in self.attachment_ids.iter().enumerate() {
            let label = self
                .attachment_infos
                .iter()
                .find(|a| &a.id == id)
                .map(|a| format!("{} · {}字节 · 移除", a.name, a.size))
                .unwrap_or_else(|| "附件信息读取中 · 移除".into());
            let id = id.clone();
            row = row.child(
                MaterialButton::new(("remove-draft-attachment", index))
                    .child(label)
                    .disabled(disabled || self.importing)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.attachment_ids.retain(|a| a != &id);
                        this.attachment_infos.retain(|a| a.id != id);
                        this.session.borrow_mut().revision += 1;
                        this.last_edit = Instant::now();
                        cx.notify();
                    })),
            );
        }
        row
    }
}

fn valid_address(address: &str) -> bool {
    !address.is_empty()
        && address.len() <= 320
        && address.matches('@').count() == 1
        && !address.starts_with('@')
        && !address.ends_with('@')
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
        && !address.contains(['<', '>', ',', ';'])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fields_separate_recipient_groups_and_preserve_incomplete_drafts() {
        let h = headers(&[
            " a@example.invalid; b@example.invalid, ".into(),
            "partial@".into(),
            "秘密@example.invalid".into(),
            "中文主题🙂".into(),
        ]);
        assert_eq!(h.to, ["a@example.invalid", "b@example.invalid"]);
        assert_eq!(h.cc, ["partial@"]);
        assert_eq!(h.bcc, ["秘密@example.invalid"]);
        assert_eq!(h.subject, "中文主题🙂");
    }
}
