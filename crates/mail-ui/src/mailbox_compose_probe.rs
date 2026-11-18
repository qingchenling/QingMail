//! 隔离的主窗口写信集成探针，不使用真实账号、密钥库或 SMTP。
use super::*;
impl Mailbox {
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
