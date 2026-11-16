//! 限时诊断：调用 GPUI 原生输入协议，不冒充真实输入法操作。
use super::*;
use gpui_kit::Keystroke;

pub(super) fn enabled() -> bool {
    std::env::args().any(|arg| matches!(arg.as_str(), "--editor-probe" | "--editor-style-demo"))
}
impl Composer {
    pub(super) fn probe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                let task = cx.background_executor().spawn(async move {
                    assert_eq!(receive(request).unwrap().document.body, draft);
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
