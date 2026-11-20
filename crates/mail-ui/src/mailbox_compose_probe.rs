//! 隔离的主窗口写信集成探针，不使用真实账号、密钥库或 SMTP。
use super::*;
impl Mailbox {
    #[cfg(debug_assertions)]
    pub(super) fn start_auto_read_probe(window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QINGMAIL_AUTO_READ_PROBE").is_none() {
            return;
        }
        assert!(
            std::env::var_os("QINGMAIL_IPC_ROOT").is_some(),
            "isolated root required"
        );
        cx.spawn_in(window, async move |entity, cx| {
            let started = std::time::Instant::now();
            let mut step = 0;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                let done = entity.update_in(cx, |this, _, cx| {
                    assert!(started.elapsed().as_secs() < 30, "auto read timeout step={step}: {}", this.status);
                    if this.busy { return false; }
                    match step {
                        0 => {
                            assert!(this.messages().len() >= 4);
                            this.checked.insert(this.messages()[1].location.clone());
                            this.select_message(0, cx);
                        }
                        1 => {
                            if !this.messages()[0].seen { return false; }
                            assert!(!this.messages()[1].seen, "checked mail must remain unread");
                            assert_eq!(this.inline.as_ref().unwrap().location.as_ref().unwrap().uid, 240);
                            this.checked.clear();
                            this.apply_flags(Some(false), None, cx);
                        }
                        2 => {
                            assert!(!this.messages()[0].seen, "explicit mark unread must survive refresh");
                            this.quiet_reload = true;
                            this.load_cache(cx);
                        }
                        3 => {
                            assert!(!this.messages()[0].seen);
                            this.select_message(0, cx);
                        }
                        4 => {
                            if !this.messages()[0].seen { return false; }
                            this.select_message(3, cx);
                        }
                        5 => {
                            if this.read_running { return false; }
                            assert!(!this.messages()[3].seen, "failed body read must remain unread");
                            assert!(this.reading_status.contains("无法加载"));
                            this.select_message(1, cx);
                            this.select_message(2, cx);
                        }
                        6 => {
                            if !this.messages()[2].seen { return false; }
                            assert!(!this.messages()[1].seen, "abandoned read must remain unread");
                            this.search_results = Some(vec![mail_storage::SearchHit {
                                message: this.messages()[1].clone(), folder: "INBOX".into(),
                            }]);
                            this.open_hit(0, cx);
                        }
                        _ => {
                            if !this.search_results.as_ref().unwrap()[0].message.seen { return false; }
                            assert!(this.messages()[1].seen);
                            assert!(!this.messages()[3].seen);
                            assert_eq!(cx.windows().len(), 1);
                            eprintln!("AUTO_READ PASS displayed_only checked_untouched manual_unread refresh reopen failed_body cancelled_read search offline_queue");
                            crate::lifecycle::close_frontend(cx);
                            return true;
                        }
                    }
                    eprintln!("AUTO_READ step={step}");
                    step += 1;
                    cx.notify();
                    false
                }).unwrap();
                if done { break; }
            }
        }).detach();
    }
    #[cfg(debug_assertions)]
    pub(super) fn start_sent_confirmation_probe(window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QINGMAIL_SENT_CONFIRMATION_PROBE").is_none() {
            return;
        }
        assert!(
            std::env::var_os("QINGMAIL_IPC_ROOT").is_some(),
            "isolated root required"
        );
        cx.spawn_in(window, async move |entity, cx| {
            let started = std::time::Instant::now();
            let mut step = 0;
            let mut tasks = None;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(180)).await;
                let request = entity.update_in(cx, |this, _, _| {
                    if tasks.is_none() && !this.busy {
                        this.account.as_ref().map(|(account, _)| this.store.tasks(account.clone()))
                    } else { None }
                }).unwrap();
                if let Some(request) = request {
                    tasks = Some(cx.background_executor().spawn(async move { receive(request) }).await.unwrap());
                }
                let done = entity.update_in(cx, |this, window, cx| {
                    assert!(started.elapsed().as_secs() < 30, "sent confirmation timeout step={step}");
                    assert_eq!(cx.windows().len(), 1);
                    let Some(tasks) = &tasks else { return false; };
                    if this.busy || this.sync_refreshing { return false; }
                    let saved = tasks.iter().find(|task| task.copy.is_some()).unwrap();
                    let local = tasks.iter().find(|task| task.copy.is_none() && task.state == mail_core::SendState::Sent).unwrap();
                    let pending = tasks.iter().find(|task| task.state == mail_core::SendState::CopyPending).unwrap();
                    match step {
                        0 => this.show_sent_confirmation(saved.clone(), cx),
                        1 => {
                            let confirmation = this.sent_confirmation.as_ref().unwrap();
                            assert!(confirmation.error.is_none(), "{:?}", confirmation.error);
                            if confirmation.body_preview.is_none() { return false; }
                            assert_eq!(this.selected_folder.as_deref(), Some("Sent"));
                            assert_eq!(this.selected_location().unwrap().uid, 11);
                            this.show_sent_confirmation(pending.clone(), cx);
                        }
                        2 => {
                            let confirmation = this.sent_confirmation.as_ref().unwrap();
                            if confirmation.body_preview.is_none() { return false; }
                            assert_eq!(confirmation.details.as_ref().unwrap().state, mail_core::SendState::CopyPending);
                            assert!(this.selected.is_none());
                            window.resize(size(px(900.), px(620.)));
                            this.show_sent_confirmation(local.clone(), cx);
                        }
                        3 => {
                            let confirmation = this.sent_confirmation.as_ref().unwrap();
                            if confirmation.body_preview.is_none() { return false; }
                            assert!(this.selected.is_none());
                            assert!(this.narrow_detail);
                            this.open_local_sent_record(local.account.clone(), local.id.clone(), cx);
                        }
                        4 => {
                            if this.inline.is_none() { return false; }
                            assert!(this.inline.as_ref().unwrap().location.is_none());
                            this.show_sent_confirmation(saved.clone(), cx);
                        }
                        5 => {
                            if this.sent_confirmation.as_ref().unwrap().body_preview.is_none() { return false; }
                            this.continue_writing_after_send(window, cx);
                        }
                        6 => {
                            let Some(editor) = &this.composer else { return false; };
                            assert!(this.sent_confirmation.is_none());
                            assert_eq!(editor.read(cx).account_id(), Some(&saved.account));
                            this.leave_composer(cx);
                        }
                        7 => {
                            if this.composer.is_some() { return false; }
                            this.show_sent_confirmation(saved.clone(), cx);
                        }
                        8 => {
                            this.return_to_inbox_after_send(cx);
                        }
                        _ => {
                            assert!(this.sent_confirmation.is_none());
                            assert_eq!(this.selected_folder.as_deref(), Some("INBOX"));
                            eprintln!("SENT_CONFIRMATION PASS saved_copy pending_copy local_record narrow compose inbox one_window no_network_send");
                            crate::lifecycle::close_frontend(cx);
                            return true;
                        }
                    }
                    eprintln!("SENT_CONFIRMATION step={step}");
                    step += 1;
                    cx.notify();
                    false
                }).unwrap();
                if done { break; }
            }
        }).detach();
    }
    pub(super) fn start_motion_probe(window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QINGMAIL_MOTION_PROBE").is_none() {
            return;
        }
        assert!(
            std::env::var_os("QINGMAIL_IPC_ROOT").is_some(),
            "isolated root required"
        );
        cx.spawn_in(window, async move |entity, cx| {
            for step in 0..14 {
                cx.background_executor().timer(std::time::Duration::from_millis(700)).await;
                entity.update_in(cx, |this, window, cx| {
                    assert!(!this.accounts.is_empty(), "fixture must load");
                    let account = this.accounts[0].0.0.clone();
                    match step {
                        0 => { this.closed_accounts.insert(account); }
                        1 => { this.closed_accounts.clear(); }
                        2 => {
                            this.filters_open = true;
                            this.select_message(0, cx);
                        }
                        3 => { this.filters_open = false; this.select_message(1, cx); }
                        4 => { this.show_page(Page::Settings, window, cx); }
                        5 | 6 => {
                            let mut p = crate::settings::prefs(cx);
                            p.dark = !p.dark;
                            crate::settings::set(cx, p);
                        }
                        7 => {
                            assert!(!crate::settings::theme_running(cx), "theme must settle");
                            this.show_page(Page::Accounts, window, cx);
                        }
                        8 => {
                            let mut p = crate::settings::prefs(cx);
                            p.reduce_motion = true;
                            crate::settings::set(cx, p);
                            this.show_page(Page::Mail, window, cx);
                            this.closed_accounts.insert(account);
                        }
                        9 => { this.closed_accounts.clear(); }
                        10 => {
                            let mut p = crate::settings::prefs(cx);
                            p.dark = true;
                            crate::settings::set(cx, p);
                            assert!(!crate::settings::theme_running(cx));
                            assert!(gpui_kit_material::theme::is_dark());
                        }
                        11 => {
                            let mut p = crate::settings::prefs(cx);
                            p.reduce_motion = false;
                            crate::settings::set(cx, p);
                        }
                        12 => { this.closed_accounts.insert(account); }
                        _ => {
                            this.closed_accounts.clear();
                            eprintln!("MOTION_UI PASS disclosure selection navigation theme reduced_motion resumed");
                            crate::lifecycle::close_frontend(cx);
                        }
                    }
                    cx.notify();
                }).expect("motion host");
                if step == 1 || step == 5 {
                    // Interrupt before the spring settles, then redirect again.
                    for reverse in [true, false] {
                        cx.background_executor().timer(std::time::Duration::from_millis(70)).await;
                        entity.update_in(cx, |this, _, cx| {
                            if step == 1 {
                                if reverse { this.closed_accounts.insert(this.accounts[0].0.0.clone()); }
                                else { this.closed_accounts.clear(); }
                            } else {
                                let mut p = crate::settings::prefs(cx);
                                p.dark = !p.dark;
                                crate::settings::set(cx, p);
                            }
                            cx.notify();
                        }).expect("motion reversal host");
                    }
                }
            }
        }).detach();
    }
    pub(super) fn start_drafts_probe(window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QINGMAIL_DRAFT_WORKSPACE_PROBE").is_none() {
            return;
        }
        assert!(
            std::env::var_os("QINGMAIL_IPC_ROOT").is_some(),
            "isolated root required"
        );
        cx.spawn_in(window, async move |entity, cx| {
            let started = std::time::Instant::now();
            let mut step = 0;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                let done = entity.update_in(cx, |this, window, cx| {
                    assert!(started.elapsed().as_secs() < 45, "draft workspace timeout step={step}");
                    assert_eq!(cx.windows().len(), 1);
                    if this.busy { return false; }
                    if step == 0 {
                        let folder = this.folders.iter().find(|f| is_drafts_folder(&f.display_name)).expect("fixture drafts").name.clone();
                        this.select_folder(folder, cx);
                    } else if step <= 10 {
                        if step == 3 {
                            // No popup opening or manual refresh: the bottom icon
                            // must discover a newly saved draft by itself.
                            if this.sync_state.summary().drafts == 0 { return false; }
                            assert!(!this.sync_open);
                        }
                        if step == 8 && !this.sync_open { this.open_sync_status(cx); }
                        let Some(editor) = &this.composer else { return false; };
                        assert_eq!(this.page, Page::Drafts);
                        if !editor.update(cx, |c, cx| c.draft_workspace_probe_step(step - 1, window, cx)) { return false; }
                    } else if step == 11 {
                        // The folder path must save/close the whole workspace; it must
                        // not nest another three-column editor inside the reader.
                        this.select_folder("INBOX".into(), cx);
                    } else {
                        if this.composer.is_some() || this.sync_refreshing || this.sync_state.summary().pending() != 0 { return false; }
                        assert!(this.sync_state.loaded && this.sync_state.error.is_none());
                        assert!(this.sync_open, "open popup must clear without reopening or manual refresh");
                        assert_eq!(this.sync_state.indicator().0, "check_circle");
                        assert_eq!(this.page, Page::Mail);
                        eprintln!("SYNC_STATUS PASS closed_icon_detected_draft open_popup_cleared_automatically");
                        eprintln!("DRAFT_WORKSPACE PASS empty new dirty_switch close_reopen delete next last dirty_delete folder_exit one_window");
                        crate::lifecycle::close_frontend(cx);
                        return true;
                    }
                    eprintln!("DRAFT_WORKSPACE step={step} done");
                    step += 1;
                    false
                }).expect("draft workspace host");
                if done { break; }
            }
        }).detach();
    }
    pub(super) fn start_compose_probe(window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("QINGMAIL_INLINE_COMPOSE_PROBE").is_none() {
            return;
        }
        let root = std::path::PathBuf::from(
            std::env::var_os("QINGMAIL_IPC_ROOT").expect("isolated IPC root required"),
        );
        cx.spawn_in(window, async move |entity, cx| {
            let started = std::time::Instant::now();
            let mut step = 0;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                let done = entity.update_in(cx, |this, window, cx| {
                    assert!(started.elapsed().as_secs() < 80, "inline probe timeout step={step}");
                    assert_eq!(cx.windows().len(), 1, "compose must never open a second window");
                    if this.busy {return false;}
                    match step {
                        0 | 14 => {
                            let (account, settings) = this.account.clone().expect("fixture account");
                            this.compose(Some((account, settings.address)), None, false, window, cx);
                        }
                        1 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(0, window, cx)) {return false;} }
                        2 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(12, window, cx)) {return false;} }
                        3 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(13, window, cx)) {return false;} }
                        4 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(1, window, cx)) {return false;} }
                        5 => {
                            let c = this.composer.as_ref().unwrap();
                            if !c.read(cx).inline_probe_review_ready() {return false;}
                            c.update(cx, |c, cx| c.inline_probe_cancel_review(cx));
                        }
                        6 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(14, window, cx)) {return false;} }
                        7 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(3, window, cx)) {return false;} }
                        8 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(4, window, cx)) {return false;} }
                        9 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(5, window, cx)) {return false;} }
                        10 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(6, window, cx)) {return false;} }
                        11 => {this.select_message(0, cx);}
                        12 => {if this.composer.is_some() || this.read_running {return false;} assert!(this.inline.is_some());}
                        13 => {window.resize(size(px(900.), px(760.)));}
                        15 => {if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(7, window, cx)) {return false;} eprintln!("INLINE_COMPOSE READY_FAILURE");}
                        16 => {
                            if !root.join("fail-save").exists() {return false;}
                            this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(8, window, cx));
                        }
                        17 => {
                            if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(9, window, cx)) {return false;}
                            assert!(this.pending_compose.is_none());
                            eprintln!("INLINE_COMPOSE FAILURE_RETAINED");
                        }
                        18 => {
                            if !root.join("recover-save").exists() {return false;}
                            this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(10, window, cx));
                        }
                        19 => {
                            if !this.composer.as_ref().unwrap().update(cx, |c, cx| c.inline_probe_step(11, window, cx)) {return false;}
                            eprintln!("INLINE_COMPOSE PASS one_window format_history IME send_review_cancel multi_draft selection_close reopen narrow failure_retained exit_flush");
                            crate::lifecycle::close_frontend(cx);
                            return true;
                        }
                        _ => unreachable!(),
                    }
                    eprintln!("INLINE_COMPOSE step={step} done");
                    step += 1;
                    false
                }).expect("inline compose host alive");
                if done {break;}
            }
        }).detach();
    }
}
