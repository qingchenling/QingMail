//! 通用账号接入；前台仅暂存密码并传引用，检查/保存复用后台命令；保存由用户明确触发。
use gpui_kit::{
    App, Context, Entity, EntityInputHandler, Window,
    base::input::{InputEvent, InputState},
    prelude::*,
};
use mail_core::{AccountId, AccountSettings, Authentication, ConnectionSecurity, ServerSettings};
use mail_daemon::storage::receive;
use mail_service::{
    accounts::{self, RemoteFolder},
    credentials::{CredentialVault, Credentials, SystemVault},
};

use mail_protocol::{Command, Request, transport::request_id};

mod flow;
#[path = "accounts_view.rs"]
mod view;

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
    authentication: Authentication,
    oauth_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
    loaded: bool,
    form_initialized: bool,
    editing: Option<AccountId>,
    impact: Option<(AccountId, mail_storage::AccountImpact)>,
}
pub(super) fn open(cx: &mut App) {
    crate::lifecycle::open_page(crate::mailbox::Page::Accounts, cx);
}
impl Accounts {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            authentication: Authentication::Password,
            oauth_cancel: None,
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
            loaded: false,
            form_initialized: false,
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
                for step in 0..8 {
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
            3 => {
                assert!(
                    self.checked.is_none(),
                    "editing must discard checked credentials"
                );
                self.inputs[3].update(cx, |input, cx| input.set_value("0", window, cx));
                assert!(self.settings(cx).is_err());
                self.check(window, cx);
                assert!(!self.busy);
                assert!(!self.can_save(cx));
            }
            4 => {
                self.authentication = Authentication::MicrosoftPersonal;
                self.inputs[1].update(cx, |input, cx| {
                    input.set_value("probe@outlook.com", window, cx)
                });
                let settings = self.settings(cx).unwrap();
                assert_eq!(settings.imap.host, "outlook.office365.com");
                assert_eq!(settings.smtp.host, "smtp-mail.outlook.com");
                assert!(!self.can_save(cx));
            }
            5 => {
                self.authentication = Authentication::Microsoft365;
                assert_eq!(self.settings(cx).unwrap().smtp.host, "smtp.office365.com");
                self.checked = Some(Checked {
                    settings: self.settings(cx).unwrap(),
                    token: "b".repeat(32),
                });
                assert!(self.can_save(cx));
            }
            6 => {
                self.inputs[1].update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..17), "changed@outlook.com", window, cx)
                });
                assert!(!self.can_save(cx));
            }
            _ => {
                assert!(self.checked.is_none());
                eprintln!(
                    "QINGMAIL_ACCOUNT_PROBE: PASS · generic / Outlook / Microsoft365 profiles / built-in client ID / input binding / invalidation / no network or credential writes"
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
        if self.authentication != Authentication::Password {
            return mail_service::microsoft::settings(
                &self.value(0, cx),
                self.value(1, cx).trim(),
                self.authentication,
            )
            .map_err(|e| e.to_string());
        }
        let server = |offset: usize, security| {
            Ok::<_, String>(ServerSettings {
                authentication: Default::default(),
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
        self.saving = true;
        let store = crate::lifecycle::storage(cx);
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.saving = false;
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
                        this.loaded = true;
                        if this
                            .editing
                            .as_ref()
                            .is_some_and(|id| !this.saved.iter().any(|(saved, _)| saved == id))
                        {
                            this.editing = None;
                            this.impact = None;
                            this.form_initialized = false;
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
        if crate::lifecycle::vault_is_unlocking(cx) {
            self.status = "正在等待系统密钥库解锁，请在系统对话框中完成".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.vault = "检查中…".into();
        self.status = "正在检查系统密钥库…".into();
        let cancel = crate::lifecycle::vault_cancellation(cx);
        let task = cx.background_executor().spawn(async move {
            SystemVault.request_unlock(&cancel)?;
            SystemVault.status()
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                if result.is_ok() {
                    crate::lifecycle::vault_unlocked(cx);
                }
                this.vault = result
                    .map(|()| "可用，保存时将再次检查".into())
                    .unwrap_or_else(|e| e.to_string());
                this.status = format!("系统密钥库：{}", this.vault);
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
        if self.authentication != Authentication::Password {
            self.check_microsoft(cx);
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
                this.checked_result(settings, result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn checked_result(
        &mut self,
        settings: AccountSettings,
        result: Result<mail_protocol::AccountCheckResult, String>,
    ) {
        match result {
            Ok(result) => {
                self.smtp = result
                    .smtp
                    .map(|()| "连接与认证通过".into())
                    .unwrap_or_else(|e| e.message);
                match result.imap {
                    Ok(folders) => {
                        self.imap = format!("连接与认证通过 · {} 个文件夹", folders.len());
                        self.folders = folders;
                    }
                    Err(e) => self.imap = e.message,
                }
                if let Some(token) = result.check {
                    self.checked = Some(Checked { settings, token });
                    self.status = "检查通过，可以保存；修改配置后需重新检查".into();
                } else {
                    self.status = format!("未保存账号。收信：{}；发信：{}", self.imap, self.smtp);
                }
            }
            Err(e) => self.status = format!("连接检查未完成：{e}；请检查配置后重试"),
        }
    }
    fn check_microsoft(&mut self, cx: &mut Context<Self>) {
        use flow::Endpoint;
        let settings = match self.settings(cx) {
            Ok(s) => s,
            Err(e) => {
                self.status = e;
                cx.notify();
                return;
            }
        };
        let id = match request_id() {
            Ok(id) => id,
            Err(e) => {
                self.status = e.to_string();
                cx.notify();
                return;
            }
        };
        self.checked = None;
        self.folders.clear();
        self.busy = true;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.oauth_cancel = Some(cancel.clone());
        self.status = "正在启动系统浏览器登录…".into();
        let backend = crate::lifecycle::account_backend(cx);
        let address = settings.address.clone();
        let authentication = settings.imap.authentication;
        let session_id = id.clone();
        let cleanup_backend = backend.clone();
        let task = cx.background_executor().spawn(async move {
            let result = backend
                .call(Request::new(
                    id,
                    Command::BeginMicrosoftLogin {
                        address,
                        authentication,
                    },
                ))
                .map_err(flow::message);
            if result.is_err() {
                // A lost begin response can still have started a listener. Cancel its
                // known session ID; never repeat the authorization request blindly.
                let _ = backend.call(Request::new(
                    request_id().unwrap_or_else(|_| session_id.clone()),
                    Command::CancelMicrosoftLogin {
                        session: session_id.clone(),
                    },
                ));
            }
            (session_id, result)
        });
        cx.spawn(async move |entity, cx| {
            let (session_id, result) = task.await;
            let updated = entity.update(cx, |this, cx| match result {
                Ok(mail_protocol::Reply::MicrosoftLoginStarted {
                    session,
                    authorization_url,
                }) => {
                    if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                        cx.open_url(&authorization_url);
                    }
                    this.status =
                        "请在浏览器完成微软授权；返回后会检查收发连接，不会发送邮件".into();
                    this.wait_microsoft(settings, session, cancel, cx);
                }
                other => {
                    this.busy = false;
                    this.oauth_cancel = None;
                    this.status = other
                        .err()
                        .unwrap_or_else(|| "后台返回了非预期登录结果".into());
                    cx.notify();
                }
            });
            if updated.is_err() {
                cx.background_executor()
                    .spawn(async move {
                        let _ = cleanup_backend.call(Request::new(
                            request_id().unwrap_or_else(|_| session_id.clone()),
                            Command::CancelMicrosoftLogin {
                                session: session_id,
                            },
                        ));
                    })
                    .detach();
            }
        })
        .detach();
        cx.notify();
    }
    fn wait_microsoft(
        &mut self,
        settings: AccountSettings,
        session: String,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let backend = crate::lifecycle::account_backend(cx);
        let account = self.editing.clone();
        let task = cx.background_executor().spawn(async move {
            let result = flow::microsoft(
                &*backend,
                &SystemVault,
                account,
                settings.clone(),
                &session,
                &cancel,
            );
            (settings, result)
        });
        cx.spawn(async move |entity, cx| {
            let (settings, result) = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.oauth_cancel = None;
                this.checked_result(settings, result);
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
                            Ok(ref id) if updated => {
                                this.editing = Some(id.clone());
                                "账号配置与凭据已更新；本地缓存未改动".into()
                            }
                            Ok(id) => {
                                this.editing = Some(id);
                                "账号已保存；可在主界面同步邮件".into()
                            }
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
        if self.busy || self.pending.is_some() {
            return;
        }
        self.form_initialized = true;
        self.authentication = settings.imap.authentication;
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
        self.status = if self.authentication == Authentication::Password {
            "编辑账号：请输入密码并重新检查连接后才可更新"
        } else {
            "微软账号：点击浏览器登录可更新授权，原有凭据保留至保存成功"
        }
        .into();
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
                        this.loaded = false;
                        this.form_initialized = false;
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

impl Drop for Accounts {
    fn drop(&mut self) {
        if let Some(cancel) = &self.oauth_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}
