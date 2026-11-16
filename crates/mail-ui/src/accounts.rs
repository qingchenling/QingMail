//! 通用账号接入；检查不发送邮件，保存必须由用户明确触发。
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
    credentials: Credentials,
}
pub(super) struct Accounts {
    inputs: Vec<Entity<InputState>>,
    _subscriptions: Vec<gpui_kit::Subscription>,
    security: [ConnectionSecurity; 2],
    busy: bool,
    pub(super) saving: bool,
    checked: Option<Checked>,
    status: String,
    vault: String,
    imap: String,
    smtp: String,
    folders: Vec<RemoteFolder>,
    saved: Vec<(AccountId, AccountSettings)>,
    expanded: Option<AccountId>,
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
            status: "填写邮箱服务商提供的 IMAP / SMTP 配置".into(),
            vault: "尚未检查".into(),
            imap: "尚未检查".into(),
            smtp: "尚未检查".into(),
            folders: vec![],
            saved: vec![],
            expanded: None,
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
                    credentials: Credentials::new("synthetic-imap".into(), "synthetic-smtp".into())
                        .unwrap(),
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
    fn can_save(&self, cx: &App) -> bool {
        !self.busy
            && self
                .checked
                .as_ref()
                .is_some_and(|c| self.settings(cx).as_ref() == Ok(&c.settings))
            && self.value(5, cx).is_empty()
            && self.value(9, cx).is_empty()
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let request = crate::lifecycle::storage(cx).accounts();
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(saved) => this.saved = saved,
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
        if !self.saved.is_empty() {
            self.status = "本阶段支持一个邮箱，账号编辑与多账号接入将在后续提供".into();
            cx.notify();
            return;
        }
        if self.busy {
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
        let task = cx.background_executor().spawn(async move {
            let result = accounts::check(&settings, &credentials);
            (settings, credentials, result)
        });
        cx.spawn(async move |entity, cx| {
            let (settings, credentials, result) = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.smtp = result
                    .smtp
                    .map(|()| "连接与认证通过".into())
                    .unwrap_or_else(|e| e.to_string());
                let passed = result.passed();
                match result.imap {
                    Ok(folders) => {
                        this.imap = format!("连接与认证通过 · {} 个文件夹", folders.len());
                        this.folders = folders;
                    }
                    Err(e) => this.imap = e.to_string(),
                }
                if passed {
                    this.checked = Some(Checked {
                        settings,
                        credentials,
                    });
                    this.status = "检查通过。确认配置后点击“保存账号”；修改配置需重新检查".into();
                } else {
                    this.status = "未保存账号。请修正配置并重新输入密码后检查".into();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            self.status = "配置有变化，请重新输入密码并检查连接".into();
            cx.notify();
            return;
        }
        let checked = self.checked.take().expect("checked");
        self.busy = true;
        self.saving = true;
        self.status = "正在保存到系统密钥库与本地账号目录…".into();
        let request =
            crate::lifecycle::storage(cx).add_account(checked.settings, checked.credentials);
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.saving = false;
                this.status = match result {
                    Ok(_) => "账号已保存，尚未同步邮件。可在左侧展开账号查看状态".into(),
                    Err(e) => format!("保存未确认：{e}。重试前请刷新账号列表"),
                };
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
            let id = id.clone();
            tree = tree.child(
                MaterialButton::new(("account", index))
                    .child(format!(
                        "{} {}",
                        if expanded { "▾" } else { "▸" },
                        settings.display_name
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.expanded = if this.expanded.as_ref() == Some(&id) {
                            None
                        } else {
                            Some(id.clone())
                        };
                        cx.notify();
                    })),
            );
            if expanded {
                tree = tree
                    .child(div().text_sm().child(settings.address.clone()))
                    .child(div().text_sm().child("文件夹与缓存状态请在邮件窗口查看"))
                    .child(
                        MaterialButton::new(("open-mailbox", index))
                            .child("打开邮件")
                            .on_click(|_, _, cx| crate::mailbox::open(cx)),
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
            .child(div().text_size(px(24.)).child("添加通用邮箱"))
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
                                .disabled(self.busy)
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
                                .disabled(self.busy)
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
                .disabled(self.busy);
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
                            .disabled(self.busy)
                            .child("检查连接")
                            .on_click(cx.listener(|this, _, window, cx| this.check(window, cx))),
                    )
                    .child(
                        MaterialButton::new("check-vault")
                            .disabled(self.busy)
                            .child("检查密钥库")
                            .on_click(cx.listener(|this, _, _, cx| this.vault_status(cx))),
                    )
                    .child(
                        MaterialButton::new("save-account")
                            .disabled(!self.can_save(cx))
                            .child("保存账号")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .child(format!("IMAP：{}", self.imap))
            .child(format!("SMTP：{}", self.smtp))
            .child(format!("系统密钥库：{}", self.vault))
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
