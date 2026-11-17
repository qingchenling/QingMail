//! 通用账号接入；前台仅暂存密码并传引用，检查/保存复用后台命令；保存由用户明确触发。
use gpui_kit::{
    App, Bounds, Context, Entity, EntityInputHandler, Render, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
    base::input::{InputEvent, InputState},
    div,
    prelude::*,
    px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, AccountSettings, ConnectionSecurity, ServerSettings};
use mail_daemon::storage::receive;
use mail_service::{
    accounts::{self, RemoteFolder},
    credentials::{CredentialVault, Credentials, SystemVault},
};

use mail_protocol::{Command, Request, transport::request_id};

mod flow;

pub(super) type PendingSave = mail_storage::PendingAccountRequest;

const LABELS: [&str; 10] = [
    "账号名称",
    "邮箱地址",
    "IMAP 服务器",
    "IMAP 端口",
    "IMAP 用户名",
    "IMAP 密码 / 应用专用密码",
    "SMTP 服务器",
    "SMTP 端口",
    "SMTP 用户名",
    "SMTP 密码 / 应用专用密码",
];
struct Checked {
    settings: AccountSettings,
    token: String,
}
pub(super) struct Accounts {
    inputs: Vec<Entity<InputState>>,
    _subscriptions: Vec<gpui_kit::Subscription>,
    security: [ConnectionSecurity; 2],
    busy: bool,
    pub(super) saving: bool,
    checked: Option<Checked>,
    pending: Option<PendingSave>,
    status: String,
    vault: String,
    imap: String,
    smtp: String,
    folders: Vec<RemoteFolder>,
    saved: Vec<(AccountId, AccountSettings)>,
    default_account: Option<AccountId>,
    paused_accounts: Vec<AccountId>,
    expanded: Option<AccountId>,
    editing: Option<AccountId>,
    impact: Option<(AccountId, mail_storage::AccountImpact)>,
}
pub(super) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_accounts(cx) {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1120.), px(850.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 添加邮箱".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Accounts::new(window, cx))
    }) {
        Ok((window, entity)) => crate::lifecycle::register_accounts(window, entity.downgrade(), cx),
        Err(_) => eprintln!("无法打开账号窗口"),
    }
}
impl Accounts {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let inputs = (0..10)
            .map(|i| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(match i {
                            3 => "993",
                            7 => "465",
                            _ => "",
                        })
                        .masked(i == 5 || i == 9)
                })
            })
            .collect();
        let mut this = Self {
            inputs,
            _subscriptions: vec![],
            security: [ConnectionSecurity::Tls; 2],
            busy: false,
            saving: false,
            checked: None,
            pending: crate::lifecycle::pending_account_save(cx),
            status: "填写邮箱服务商提供的 IMAP / SMTP 配置".into(),
            vault: "尚未检查".into(),
            imap: "尚未检查".into(),
            smtp: "尚未检查".into(),
            folders: vec![],
            saved: vec![],
            default_account: None,
            paused_accounts: vec![],
            expanded: None,
            editing: None,
            impact: None,
        };
        this._subscriptions = this
            .inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) && !this.busy {
                        if this.checked.take().is_some() {
                            this.status = "配置已修改，请重新输入密码并检查连接".into();
                            this.folders.clear();
                            this.imap = "待重新检查".into();
                            this.smtp = "待重新检查".into();
                        }
                        cx.notify();
                    }
                })
            })
            .collect();
        if this.pending.is_some() {
            this.status = "上次保存结果待确认，请点击“查询保存结果”".into();
        }
        this.refresh(cx);
        if std::env::args().any(|a| a == "--accounts-probe") {
            cx.spawn_in(window, async move |entity, cx| {
                for step in 0..4 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    entity
                        .update_in(cx, |this, window, cx| this.probe(step, window, cx))
                        .expect("account probe window");
                }
            })
            .detach();
        }

        let entity = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            entity
                .update(cx, |this, cx| {
                    if this.saving {
                        this.status = "正在保存，请等待结果后关闭".into();
                        cx.notify();
                        false
                    } else {
                        true
                    }
                })
                .unwrap_or(true)
        });
        this
    }
    /// 仅合成输入与状态绑定探针，不进行协议检查或密钥写入。
    fn probe(&mut self, step: u8, window: &mut Window, cx: &mut Context<Self>) {
        match step {
            0 => {
                assert!(!self.can_save(cx));
                self.check(window, cx);
                assert!(!self.busy);
                for (i, value) in [
                    "合成账号",
                    "probe@example.invalid",
                    "localhost",
                    "993",
                    "probe",
                    "synthetic-imap",
                    "localhost",
                    "465",
                    "probe",
                    "synthetic-smtp",
                ]
                .iter()
                .enumerate()
                {
                    self.inputs[i].update(cx, |input, cx| input.set_value(*value, window, cx));
                }
                assert!(self.settings(cx).is_ok());
                assert_eq!(self.value(5, cx), "synthetic-imap");
            }
            1 => {
                for i in [5, 9] {
                    self.inputs[i].update(cx, |input, cx| input.set_value("", window, cx));
                }
            }
            2 => {
                self.checked = Some(Checked {
                    settings: self.settings(cx).unwrap(),
                    token: "a".repeat(32),
                });
                assert!(self.can_save(cx));
                self.inputs[2].update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..9), "changed.invalid", window, cx)
                });
                assert!(!self.can_save(cx));
            }
            _ => {
                assert!(
                    self.checked.is_none(),
                    "editing must discard checked credentials"
                );
                self.inputs[3].update(cx, |input, cx| input.set_value("0", window, cx));
                assert!(self.settings(cx).is_err());
                self.check(window, cx);
                assert!(!self.busy);
                assert!(!self.can_save(cx));
                eprintln!(
                    "QINGMAIL_ACCOUNT_PROBE: PASS · input values / validation / settings binding / invalidation / no network or credential writes"
                );
                cx.quit();
            }
        }
        cx.notify();
    }
    fn value(&self, index: usize, cx: &App) -> String {
        self.inputs[index].read(cx).value().to_string()
    }
    fn settings(&self, cx: &App) -> Result<AccountSettings, String> {
        let server = |offset: usize, security| {
            Ok::<_, String>(ServerSettings {
                host: self.value(offset, cx).trim().into(),
                port: self
                    .value(offset + 1, cx)
                    .trim()
                    .parse()
                    .map_err(|_| "端口应为 1–65535 的整数".to_owned())?,
                username: self.value(offset + 2, cx),
                security,
            })
        };
        let settings = AccountSettings {
            display_name: self.value(0, cx).trim().into(),
            address: self.value(1, cx).trim().into(),
            imap: server(2, self.security[0])?,
            smtp: server(6, self.security[1])?,
        };
        accounts::validate(&settings)
            .map_err(|_| "请填写完整的账号、邮箱地址、服务器与用户名".to_owned())?;
        Ok(settings)
    }
    fn change_option(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = crate::lifecycle::storage(cx);
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.status = match result {
                    Ok(_) => "账号设置已保存".into(),
                    Err(e) => e,
                };
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }
    fn can_save(&self, cx: &App) -> bool {
        !self.busy
            && self.pending.is_none()
            && self
                .checked
                .as_ref()
                .is_some_and(|c| self.settings(cx).as_ref() == Ok(&c.settings))
            && self.value(5, cx).is_empty()
            && self.value(9, cx).is_empty()
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = crate::lifecycle::storage(cx);
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let options = store.control(Command::AccountOptions)?;
            Ok::<_, String>((accounts, options))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok((saved, options)) => {
                        if let mail_protocol::Reply::AccountOptions { default, paused } = options {
                            this.default_account = default;
                            this.paused_accounts = paused;
                        }
                        this.saved = saved;
                        if this.saved.is_empty() {
                            this.editing = None;
                            this.impact = None;
                            this.expanded = None;
                        } else if this.expanded.is_none() && this.saved.len() == 1 {
                            this.expanded = Some(this.saved[0].0.clone());
                        }
                    }
                    Err(_) => this.status = "账号列表读取失败，请重试".into(),
                };
                cx.notify();
            });
        })
        .detach();
    }
    fn vault_status(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.vault = "检查中…".into();
        let task = cx
            .background_executor()
            .spawn(async { SystemVault.status() });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.vault = result
                    .map(|()| "可用，保存时将再次检查".into())
                    .unwrap_or_else(|e| e.to_string());
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.pending.is_some() {
            return;
        }
        self.checked = None;
        self.folders.clear();
        self.imap = "尚未检查".into();
        self.smtp = "尚未检查".into();
        let settings = match self.settings(cx) {
            Ok(s) => s,
            Err(e) => {
                self.status = e;
                cx.notify();
                return;
            }
        };
        let credentials = match Credentials::new(self.value(5, cx), self.value(9, cx)) {
            Ok(c) => c,
            Err(e) => {
                self.status = e.to_string();
                cx.notify();
                return;
            }
        };
        for i in [5, 9] {
            self.inputs[i].update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.busy = true;
        self.status = "正在检查加密连接与认证，请稍候…".into();
        let backend = crate::lifecycle::account_backend(cx);
        let account = self.editing.clone();
        let task = cx.background_executor().spawn(async move {
            let result = flow::check(
                &*backend,
                &SystemVault,
                account,
                settings.clone(),
                credentials,
            );
            (settings, result)
        });
        cx.spawn(async move |entity, cx| {
            let (settings, result) = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(result) => {
                        this.smtp = result
                            .smtp
                            .map(|()| "连接与认证通过".into())
                            .unwrap_or_else(|e| e.message);
                        match result.imap {
                            Ok(folders) => {
                                this.imap = format!("连接与认证通过 · {} 个文件夹", folders.len());
                                this.folders = folders;
                            }
                            Err(e) => this.imap = e.message,
                        }
                        if let Some(token) = result.check {
                            this.checked = Some(Checked { settings, token });
                            this.status =
                                "检查通过。确认配置后点击“保存账号”；修改配置需重新检查".into();
                        } else {
                            this.status = "未保存账号。请修正配置并重新输入密码后检查".into();
                        }
                    }
                    Err(e) => this.status = format!("连接检查未完成：{e}；请重新输入密码后检查"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            self.status = "配置有变化或保存结果待查询，请先检查状态".into();
            cx.notify();
            return;
        }
        let id = match request_id() {
            Ok(id) => id,
            Err(_) => {
                self.status = "无法生成保存标识，未提交".into();
                cx.notify();
                return;
            }
        };
        let checked = self.checked.take().expect("checked");
        let pending = PendingSave {
            id: id.clone(),
            updated: self.editing.is_some(),
        };
        self.pending = Some(pending.clone());
        crate::lifecycle::set_pending_account_save(Some(pending), cx);
        let request = Request::new(
            id,
            Command::CommitAccount {
                check: checked.token,
                account: self.editing.clone(),
            },
        );
        self.save_result(Some(request), cx);
    }
    fn save_result(&mut self, request: Option<Request>, cx: &mut Context<Self>) {
        if self.busy || self.pending.is_none() {
            return;
        }
        let pending = self.pending.clone().expect("pending save");
        let backend = crate::lifecycle::account_backend(cx);
        let journal = crate::lifecycle::account_requests(cx);
        self.busy = true;
        self.saving = true;
        self.status = if request.is_some() {
            "正在保存账号…"
        } else {
            "正在查询原保存请求，不会重新提交…"
        }
        .into();
        let task = cx.background_executor().spawn(async move {
            if request.is_some()
                && let Err(e) = journal.remember(&pending)
            {
                return flow::SaveResult::Unconfirmed(format!(
                    "保存标识未确认落盘，未提交账号：{e}"
                ));
            }
            let result = flow::save(&*backend, &pending.id, request);
            if matches!(result, flow::SaveResult::Finished(_))
                && let Err(e) = journal.clear(&pending.id)
            {
                return flow::SaveResult::Unconfirmed(format!(
                    "已取得结果，但恢复记录未确认清理：{e}"
                ));
            }
            result
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.saving = false;
                match result {
                    flow::SaveResult::Finished(result) => {
                        let updated = this.pending.take().is_some_and(|p| p.updated);
                        crate::lifecycle::set_pending_account_save(None, cx);
                        this.status = match result {
                            Ok(_) if updated => "账号配置与凭据已更新；本地缓存未改动".into(),
                            Ok(_) => "账号已保存；可在主界面同步邮件".into(),
                            Err(e) => format!("账号未保存：{}；请重新检查连接", e.message),
                        };
                    }
                    flow::SaveResult::Unconfirmed(note) => {
                        this.status =
                            format!("保存结果未确认：{note}。请点击“查询保存结果”，不要重复提交");
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_query(&mut self, cx: &mut Context<Self>) {
        if self.busy || crate::lifecycle::account_backend(cx).saving() {
            return;
        }
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let journal = crate::lifecycle::account_requests(cx);
        self.busy = true;
        self.saving = true;
        let job = cx
            .background_executor()
            .spawn(async move { journal.clear(&pending.id) });
        cx.spawn(async move |entity, cx| {
            let result = job.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.pending = None;
                        this.checked = None;
                        crate::lifecycle::set_pending_account_save(None, cx);
                        this.status = "已结束查询；没有重新提交。需修改账号时请重新检查连接".into();
                    }
                    Err(e) => this.status = format!("恢复记录未清理，仍保留待查询状态：{e}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn begin_edit(
        &mut self,
        account: AccountId,
        settings: &AccountSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending.is_some() {
            return;
        }
        self.editing = Some(account.clone());
        self.impact = None;
        self.checked = None;
        let values = [
            settings.display_name.clone(),
            settings.address.clone(),
            settings.imap.host.clone(),
            settings.imap.port.to_string(),
            settings.imap.username.clone(),
            String::new(),
            settings.smtp.host.clone(),
            settings.smtp.port.to_string(),
            settings.smtp.username.clone(),
            String::new(),
        ];
        for (input, value) in self.inputs.iter().zip(values) {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.security = [settings.imap.security, settings.smtp.security];
        self.folders.clear();
        self.imap = "待重新检查".into();
        self.smtp = "待重新检查".into();
        self.status = "编辑账号：请输入密码并重新检查连接后才可更新".into();
        cx.notify();
    }
    fn request_remove(&mut self, account: AccountId, cx: &mut Context<Self>) {
        if self.busy || self.pending.is_some() {
            return;
        }
        if crate::lifecycle::sending(cx).active() {
            self.status = "发送任务仍在处理；请等待结果写盘后再移除账号".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.status = "正在统计本地移除影响…".into();
        let reply = crate::lifecycle::storage(cx).account_impact(account.clone());
        let task = cx
            .background_executor()
            .spawn(async move { receive(reply) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(impact) => {
                        this.impact = Some((account, impact));
                        this.status = "确认下方影响后再移除；远端邮件不会被删除".into();
                    }
                    Err(e) => this.status = format!("无法读取移除影响：{e}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn confirm_remove(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_some() || self.busy {
            return;
        }
        let Some((account, _)) = self.impact.take() else {
            return;
        };
        if crate::lifecycle::sending(cx).active() {
            self.impact = None;
            self.status = "发送任务仍在处理；请等待结果写盘后再移除账号".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.saving = true;
        self.status = "正在移除本地账号、缓存、草稿与发送任务…".into();
        let reply = crate::lifecycle::storage(cx).remove_account(account);
        let task = cx
            .background_executor()
            .spawn(async move { receive(reply) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.saving = false;
                match result {
                    Ok(impact) => {
                        this.editing = None;
                        this.checked = None;
                        this.folders.clear();
                        this.status = format!(
                            "已从本地移除账号：缓存 {} 封、草稿 {} 份、发送任务 {} 条；远端邮件未删除",
                            impact.cached_messages, impact.drafts, impact.tasks
                        );
                    }
                    Err(e) => this.status = format!("移除未完全确认：{e}。请刷新账号列表核对"),
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for Accounts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tree = div()
            .w(px(245.))
            .h_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER))
            .child("邮箱账号");
        for (index, (id, settings)) in self.saved.iter().enumerate() {
            let expanded = self.expanded.as_ref() == Some(id);
            let row_id = id.clone();
            let expand_id = row_id.clone();
            tree = tree.child(
                MaterialButton::new(("account", index))
                    .child(format!(
                        "{} {}",
                        if expanded { "▾" } else { "▸" },
                        settings.display_name
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.expanded = if this.expanded.as_ref() == Some(&expand_id) {
                            None
                        } else {
                            Some(expand_id.clone())
                        };
                        cx.notify();
                    })),
            );
            if expanded {
                let default_id = row_id.clone();
                let pause_id = row_id.clone();
                let is_default = self.default_account.as_ref() == Some(id);
                let paused = self.paused_accounts.contains(id);
                let edit_id = row_id.clone();
                let remove_id = row_id.clone();
                let edit_settings = settings.clone();
                tree = tree
                    .child(div().text_sm().child(settings.address.clone()))
                    .child(
                        MaterialButton::new(("default-account", index))
                            .child(if is_default {
                                "默认发件账号"
                            } else {
                                "设为默认发件账号"
                            })
                            .disabled(self.busy || is_default)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change_option(
                                    Command::SetDefaultAccount {
                                        account: default_id.clone(),
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        MaterialButton::new(("pause-account", index))
                            .child(if paused {
                                "恢复此账号同步"
                            } else {
                                "暂停此账号同步"
                            })
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change_option(
                                    Command::PauseSync {
                                        account: Some(pause_id.clone()),
                                        paused: !paused,
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(div().text_sm().child("文件夹与缓存状态请在邮件窗口查看"))
                    .child(
                        MaterialButton::new(("open-mailbox", index))
                            .child("打开邮件")
                            .on_click(|_, _, cx| crate::mailbox::open(cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                MaterialButton::new(("edit-account", index))
                                    .variant(ButtonVariant::Tonal)
                                    .child("编辑配置")
                                    .disabled(self.busy || self.pending.is_some())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.begin_edit(
                                            edit_id.clone(),
                                            &edit_settings,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                MaterialButton::new(("remove-account", index))
                                    .child("移除账号")
                                    .disabled(self.busy || self.pending.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.request_remove(remove_id.clone(), cx);
                                    })),
                            ),
                    );
            }
        }
        if self.saved.is_empty() {
            tree = tree.child("尚未添加邮箱");
        }
        tree = tree.child(
            MaterialButton::new("refresh-accounts")
                .child("刷新账号列表")
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
        );
        let mut form = div()
            .id("account-form")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(px(24.)).child(if self.editing.is_some() {
                "编辑邮箱账号"
            } else {
                "添加通用邮箱"
            }))
            .child("使用服务商的密码或应用专用密码。暂不支持 OAuth 登录。")
            .child("检查仅验证连接与认证，不发送邮件。密码保存在系统密钥库。");
        for (i, label) in LABELS.iter().enumerate() {
            if i == 2 || i == 6 {
                let index = usize::from(i == 6);
                let security = self.security[index];
                form = form.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(if i == 2 { "IMAP 收信" } else { "SMTP 发信" })
                        .child(
                            MaterialButton::new(("tls", index))
                                .disabled(self.busy || self.pending.is_some())
                                .variant(if security == ConnectionSecurity::Tls {
                                    ButtonVariant::Filled
                                } else {
                                    ButtonVariant::Text
                                })
                                .child("TLS")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.security[index] = ConnectionSecurity::Tls;
                                    this.checked = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            MaterialButton::new(("starttls", index))
                                .disabled(self.busy || self.pending.is_some())
                                .variant(if security == ConnectionSecurity::StartTls {
                                    ButtonVariant::Filled
                                } else {
                                    ButtonVariant::Text
                                })
                                .child("STARTTLS")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.security[index] = ConnectionSecurity::StartTls;
                                    this.checked = None;
                                    cx.notify();
                                })),
                        ),
                );
            }
            let field = TextField::new(("account-input", i), &self.inputs[i])
                .label(*label)
                .disabled(self.busy || self.pending.is_some());
            form = form.child(if i == 5 || i == 9 {
                field.input_type(TextFieldInputType::Password)
            } else {
                field
            });
        }
        form = form
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        MaterialButton::new("check-account")
                            .disabled(self.busy || self.pending.is_some())
                            .child("检查连接")
                            .on_click(cx.listener(|this, _, window, cx| this.check(window, cx))),
                    )
                    .child(
                        MaterialButton::new("check-vault")
                            .disabled(self.busy || self.pending.is_some())
                            .child("检查密钥库")
                            .on_click(cx.listener(|this, _, _, cx| this.vault_status(cx))),
                    )
                    .child(
                        MaterialButton::new("save-account")
                            .disabled(!self.can_save(cx))
                            .child(if self.editing.is_some() {
                                "更新账号"
                            } else {
                                "保存账号"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .when(self.pending.is_some(), |form| form.child(
                div().flex().flex_col().gap_2()
                .child("请先刷新并核对账号状态；结束查询不会取消或重新执行原请求。")
                .child(MaterialButton::new("query-account-save")
                    .child("查询保存结果")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.save_result(None, cx))))
                .child(MaterialButton::new("finish-account-query")
                    .child("已核对账号，结束查询")
                    .disabled(self.busy || crate::lifecycle::account_backend(cx).saving())
                    .on_click(cx.listener(|this, _, _, cx| this.finish_query(cx))))
            ))
            .child(format!("IMAP：{}", self.imap))
            .child(format!("SMTP：{}", self.smtp))
            .child(format!("系统密钥库：{}", self.vault))
            .when(self.impact.is_some(), |form| {
                let (_, impact) = self.impact.as_ref().expect("impact");
                form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .child(format!(
                            "本地移除影响：缓存 {} 封 · 草稿 {} 份 · 发送任务 {} 条（待处理 {}）· 本地对象 {} 字节",
                            impact.cached_messages,
                            impact.drafts,
                            impact.tasks,
                            impact.pending_tasks,
                            impact.object_bytes
                        ))
                        .child("只删除本地账号与上述数据；不删除远端邮件。密钥库条目会一并尝试删除。")
                        .when(impact.pending_tasks > 0, |panel| {
                            panel.child(
                                div()
                                    .text_color(theme::color(ColorRole::ERROR))
                                    .child(format!(
                                        "有 {} 条待处理发送任务；移除会删除其记录，请先确认已无在途发送",
                                        impact.pending_tasks
                                    )),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    MaterialButton::new("confirm-remove-account")
                                        .child("确认移除")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_remove(cx);
                                        })),
                                )
                                .child(
                                    MaterialButton::new("cancel-remove-account")
                                        .child("取消")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.impact = None;
                                            this.status = "已取消移除".into();
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .child(self.status.clone());
        if !self.folders.is_empty() {
            form = form.child("本次检查发现的文件夹（只读列表，尚未同步）");
            for folder in &self.folders {
                form = form.child(format!(
                    "{}{}",
                    folder.display_name,
                    if folder.selectable {
                        ""
                    } else {
                        "（不可选）"
                    }
                ));
            }
        }
        div()
            .size_full()
            .flex()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(tree)
            .child(form)
    }
}
