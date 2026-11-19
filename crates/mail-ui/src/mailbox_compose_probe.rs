//! 隔离的主窗口写信集成探针，不使用真实账号、密钥库或 SMTP。
use super::*;
impl Mailbox {
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
