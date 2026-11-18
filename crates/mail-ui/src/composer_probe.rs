//! 限时诊断：调用 GPUI 原生输入协议，不冒充真实输入法操作。
use super::*;
use gpui_kit::Keystroke;

pub(super) fn enabled() -> bool {
    std::env::args().any(|arg| matches!(arg.as_str(), "--editor-probe" | "--editor-style-demo"))
}
impl Composer {
    pub(super) fn probe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.importing {
            return;
        }
        let step = self.probe_step;
        match step {
            0 => {
                window.activate_window();
                for family in ["sans-serif", self.font_family.as_ref()] {
                    let regular = window.text_system().resolve_font(&gpui_kit::font(family));
                    let bold = window
                        .text_system()
                        .resolve_font(&gpui_kit::font(family).bold());
                    let italic = window
                        .text_system()
                        .resolve_font(&gpui_kit::font(family).italic());
                    eprintln!(
                        "FONT_PROBE {family}: regular={regular:?} bold={bold:?} italic={italic:?}"
                    );
                    if family == self.font_family.as_ref() {
                        assert!(
                            regular != bold && regular != italic && bold != italic,
                            "editor requires separate font faces"
                        );
                    }
                }
                assert!(self.store.is_some(), "probe store must open");
                if std::env::args().any(|a| a == "--compose-restore-probe") {
                    if self.attachment_infos.len() != self.attachment_ids.len() {
                        return;
                    }
                    assert_eq!(
                        self.header_values[0],
                        "to@example.invalid, second@example.invalid"
                    );
                    assert_eq!(self.header_values[1], "cc@example.invalid");
                    assert_eq!(self.header_values[2], "bcc@example.invalid");
                    assert_eq!(self.header_values[3], "中文主题🙂（保存中修改）");
                    assert_eq!(self.attachment_infos.len(), 1);
                    assert_eq!(self.attachment_infos[0].name, "中文附件.txt");
                    assert!(
                        self.session
                            .borrow()
                            .draft
                            .text()
                            .ends_with("后台保存期间输入")
                    );
                    assert_eq!(self.saved, Some(self.session.borrow().revision));
                    eprintln!("QINGMAIL_COMPOSE_RESTORE_PROBE: PASS");
                    cx.quit();
                    return;
                }

                if std::env::args().any(|arg| arg == "--editor-style-demo") {
                    let text = "Regular: QingMail ABC 123 中文正常\nBold: QingMail ABC 123 中文粗体\nItalic: QingMail ABC 123 中文斜体\nBoth: QingMail ABC 123 中文粗斜体";
                    let mut draft = Draft::plain(text).unwrap();
                    let lines: Vec<_> = text.split('\n').collect();
                    let mut offset = lines[0].len() + 1;
                    for (index, line) in lines.iter().enumerate().skip(1) {
                        draft
                            .inline(offset..offset + line.len(), |a| {
                                a.bold = index != 2;
                                a.italic = index >= 2;
                            })
                            .unwrap();
                        offset += line.len() + 1;
                    }
                    *self.session.borrow_mut() = Session::new(draft);
                    self.sync_input(window, cx);
                    self.probe_step = 999;
                    return;
                }

                *self.session.borrow_mut() = Session::new(Draft::plain("中文🙂\n第二段").unwrap());
                self.session
                    .borrow_mut()
                    .draft
                    .inline(11..20, |a| a.italic = true)
                    .unwrap();
                self.saved = None;
                self.sync_input(window, cx);
                if std::env::args().any(|a| a == "--compose-fields-probe") {
                    for (index, value) in [
                        "to@example.invalid; second@example.invalid",
                        "cc@example.invalid",
                        "bcc@example.invalid",
                        "中文主题🙂",
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        self.header_inputs[index].update(cx, |input, cx| {
                            input.replace_text_in_range(None, value, window, cx)
                        });
                    }
                    self.importing = true;
                    let source = std::env::var_os("QINGMAIL_COMPOSE_PROBE_SOURCE")
                        .expect("isolated attachment fixture");
                    let request = self.store.as_ref().unwrap().import_attachment(
                        self.stored.as_ref().unwrap().account.clone(),
                        source.into(),
                    );
                    let task = cx
                        .background_executor()
                        .spawn(async move { receive(request) });
                    cx.spawn(async move |entity, cx| {
                        let info = task.await.expect("probe attachment import");
                        entity
                            .update(cx, |this, cx| {
                                this.importing = false;
                                this.accept_attachment(info, cx);
                            })
                            .unwrap();
                    })
                    .detach();
                }

                self.input.update(cx, |input, cx| {
                    input.focus(window, cx);
                    input.set_selected_range(0..6, cx);
                });
            }
            1 => self.format("bold", window, cx),
            2 => {
                assert!(self.session.borrow().draft.attributes()[0].bold);
                window.on_next_frame(|window, cx| {
                    window.dispatch_keystroke(Keystroke::parse("ctrl-z").unwrap(), cx);
                });
            }
            3 => {
                assert!(
                    !self.session.borrow().draft.attributes()[0].bold,
                    "keyboard undo must include formatting"
                );
                window.on_next_frame(|window, cx| {
                    window.dispatch_keystroke(Keystroke::parse("ctrl-y").unwrap(), cx);
                });
            }
            4 => {
                assert!(
                    self.session.borrow().draft.attributes()[0].bold,
                    "keyboard redo restores marks"
                );
                self.input.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(Some(0..2), "ni", Some(2..2), window, cx);
                    assert!(input.marked_text_range(window, cx).is_some());
                    assert_eq!(input.value().as_ref(), "ni🙂\n第二段");
                    input.replace_text_in_range(None, "你", window, cx);
                    assert!(input.marked_text_range(window, cx).is_none());
                });
            }
            5 => {
                assert_eq!(self.session.borrow().draft.text(), "你🙂\n第二段");
            }
            6 => {
                assert!(!self.composing(window, cx));
                assert_eq!(self.session.borrow().draft.text(), "你🙂\n第二段");
                assert!(
                    self.session.borrow().draft.attributes()[3].italic,
                    "IME full snapshot must preserve suffix formatting"
                );
                self.history(false, window, cx);
            }
            7 => {
                assert_eq!(
                    self.session.borrow().draft.text(),
                    "中文🙂\n第二段",
                    "composition undo restores original text"
                );
                assert!(self.session.borrow().draft.attributes()[0].bold);
                self.history(true, window, cx);
            }
            8 => {
                assert_eq!(self.session.borrow().draft.text(), "你🙂\n第二段");
                self.input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(1..3), "替换🙂", window, cx)
                });
            }
            9 => {
                assert_eq!(self.session.borrow().draft.text(), "你替换🙂\n第二段");
                let len = self.session.borrow().draft.text().len();
                self.input
                    .update(cx, |input, cx| input.set_selected_range(0..len, cx));
                self.format("italic", window, cx);
                self.format("bullet", window, cx);
                self.format("link", window, cx);
            }
            10 => {
                assert!(self.session.borrow().draft.html().contains("<ul><li>"));
                assert!(self.session.borrow().draft.html().contains("<em>"));
                assert!(
                    self.session
                        .borrow()
                        .draft
                        .html()
                        .contains("https://example.org/qingmail")
                );
                self.save(cx);
            }
            11 => {
                assert!(self.saving, "slow save must still be in background");
                if std::env::args().any(|a| a == "--compose-fields-probe") {
                    self.header_inputs[3].update(cx, |input, cx| {
                        let end = input.value().encode_utf16().count();
                        input.replace_text_in_range(
                            Some(0..end),
                            "中文主题🙂（保存中修改）",
                            window,
                            cx,
                        );
                    });
                }

                let end = self.session.borrow().draft.text().encode_utf16().count();
                self.input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(end..end), "后台保存期间输入", window, cx)
                });
            }
            12 => {
                assert!(
                    self.session
                        .borrow()
                        .draft
                        .text()
                        .ends_with("后台保存期间输入")
                );
                if self.saving {
                    return;
                }
                self.save(cx);
            }
            13 => {
                if self.saving || self.saved != Some(self.session.borrow().revision) {
                    return;
                }
                let store = self.store.clone().unwrap();
                let request = store.open_editor(self.session.borrow().draft.clone());
                let draft = self.session.borrow().draft.clone();
                let expected_headers = fields::headers(&self.header_values);
                let expected_attachments = self.attachment_ids.clone();
                let task = cx.background_executor().spawn(async move {
                    let restored = receive(request).unwrap();
                    assert_eq!(restored.document.body, draft);
                    assert_eq!(restored.document.headers, expected_headers);
                    assert_eq!(restored.document.attachments, expected_attachments);
                    let evidence = "PASS native layout; UTF-8 selection; UTF-16 emoji replacement; marked preedit/commit; composition undo/redo; keyboard format undo/redo; inline bold/italic/link; controlled list export; background save/load equality; input accepted during 900ms simulated slow save; old save acknowledgement does not mark new edits saved\n";
                    std::fs::write("target/p0-editor-probe-result.txt", evidence).unwrap();
                    eprintln!("QINGMAIL_EDITOR_PROBE: {evidence}");
                });
                cx.spawn(async move |_, cx| {
                    task.await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
            _ => return,
        }
        self.probe_step += 1;
    }
}

/// 主窗口嵌入式流程的原生实体探针；只由隔离测试入口调用，不确认 SMTP。
impl Composer {
    pub(crate) fn inline_probe_step(
        &mut self,
        step: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.loading || self.saving || self.checking_send || self.pending_draft.is_some() {
            return false;
        }
        // Like a real user, wait until the rendered native controls accept input.
        // Storage completion can precede the next frame that clears a TextField's disabled state.
        if [0, 4, 14].contains(&step) && !self.header_inputs[3].read(cx).is_editable() {
            return false;
        }
        if [0, 6, 8, 11, 12].contains(&step) && !self.input.read(cx).is_editable() {
            return false;
        }
        match step {
            0 => {
                self.header_inputs[0].update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "to@example.invalid", window, cx);
                });
                self.header_inputs[3].update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "内嵌草稿一", window, cx);
                });
                self.input.update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "内嵌写信 Chinese 🙂", window, cx);
                });
            }
            12 => {
                // IME veto must retain this entity and its host window.
                self.input.update(cx, |i, cx| {
                    let n = i.value().len();
                    i.set_selected_range(n..n, cx);
                    i.replace_and_mark_text_in_range(None, "候选", None, window, cx);
                });
                assert!(self.prepare_close(window, cx).is_err());
                self.input
                    .update(cx, |i, cx| i.replace_text_in_range(None, "", window, cx));
            }
            14 => {
                self.header_inputs[3].update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "内嵌草稿一 · 最新", window, cx);
                });
            }
            13 => {
                assert_eq!(self.session.borrow().draft.text(), "内嵌写信 Chinese 🙂");
                let len = self.session.borrow().draft.text().len();
                self.input
                    .update(cx, |i, cx| i.set_selected_range(0..len, cx));
                self.format("bold", window, cx);
                self.format("italic", window, cx);
                self.history(false, window, cx);
                self.history(true, window, cx);
                assert!(self.session.borrow().draft.attributes()[0].bold);
                assert!(self.session.borrow().draft.attributes()[0].italic);
                self.save(cx);
            }
            1 => {
                if self.saved != Some(self.session.borrow().revision) {
                    return false;
                }
                self.check_send(window, cx);
            }
            3 => self.switch_draft(None, window, cx),
            4 => {
                assert!(self.session.borrow().draft.text().is_empty());
                self.input.update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "第二份草稿", window, cx);
                });
                self.header_inputs[3].update(cx, |i, cx| {
                    i.set_selected_range(0..i.value().len(), cx);
                    i.replace_text_in_range(None, "内嵌草稿二", window, cx);
                });
                self.save(cx);
            }
            5 => {
                if self.saved != Some(self.session.borrow().revision) {
                    return false;
                }
                let Some(index) = self
                    .drafts
                    .iter()
                    .position(|d| d.document.headers.subject == "内嵌草稿一 · 最新")
                else {
                    return false;
                };
                self.switch_draft(Some(index), window, cx);
            }
            6 => {
                assert_eq!(self.header_values[3], "内嵌草稿一 · 最新");
                assert_eq!(self.session.borrow().draft.text(), "内嵌写信 Chinese 🙂");
                assert!(self.session.borrow().draft.attributes()[0].bold);
                assert!(self.session.borrow().draft.attributes()[0].italic);
                self.input.update(cx, |i, cx| {
                    let n = i.value().len();
                    i.set_selected_range(n..n, cx);
                    i.replace_text_in_range(None, "即时关闭", window, cx);
                });
            }
            7 => {
                assert_eq!(
                    self.session.borrow().draft.text(),
                    "内嵌写信 Chinese 🙂即时关闭"
                );
                assert!(self.session.borrow().draft.attributes()[0].bold);
            }
            8 => {
                self.input.update(cx, |i, cx| {
                    let n = i.value().len();
                    i.set_selected_range(n..n, cx);
                    i.replace_text_in_range(None, "失败保留", window, cx);
                });
                self.request_close(cx);
            }
            9 => {
                if !self.auto_save_blocked {
                    return false;
                }
                assert!(!self.closed && !self.closing);
                assert!(self.session.borrow().draft.text().ends_with("失败保留"));
            }
            10 => self.save(cx),
            11 => {
                if self.saved != Some(self.session.borrow().revision) {
                    return false;
                }
                self.input.update(cx, |i, cx| {
                    let n = i.value().len();
                    i.set_selected_range(n..n, cx);
                    i.replace_text_in_range(None, "退出保存", window, cx);
                });
            }
            _ => unreachable!(),
        }
        true
    }
    pub(crate) fn inline_probe_review_ready(&self) -> bool {
        self.send_review.is_some()
    }
    pub(crate) fn inline_probe_cancel_review(&mut self, cx: &mut Context<Self>) {
        self.cancel_review(cx);
    }
}
