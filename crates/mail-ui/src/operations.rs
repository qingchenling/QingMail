//! 持久邮件操作的状态/处理入口；不重放 SMTP 或不确定移动。
use gpui_kit::{
    App, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_daemon::storage::StorageClient;
use mail_protocol::{Command, OperationInfo, OperationResolution, OperationState as State, Reply};
pub(super) fn open(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(960.), px(720.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 待同步操作".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, |_, cx| cx.new(Operations::new)) {
        eprintln!("无法打开待同步操作：{e}");
    }
}
struct Operations {
    store: StorageClient,
    items: Vec<OperationInfo>,
    status: String,
    busy: bool,
    confirm: Option<String>,
}
impl Operations {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            items: vec![],
            status: "正在读取…".into(),
            busy: false,
            confirm: None,
        };
        this.load(None, cx);
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                if entity.update(cx, |this, cx| this.load(None, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }
    fn load(&mut self, command: Option<Command>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(command) = command {
                store.control(command)?;
            }
            match store.control(Command::Operations { account: None })? {
                Reply::Operations(items) => Ok(items),
                _ => Err("操作状态响应无效".to_string()),
            }
        });
        cx.spawn(async move|entity,cx|{
            let result=task.await;
            let _=entity.update(cx,|this,cx|{this.busy=false;match result {Ok(items)=>{this.items=items;this.status="显示最近200条。暂停账号会暂停其队列；恢复网络不会自动发送草稿。移动不确定时请查看源/目标两端，再明确保留服务器状态。".into();},Err(e)=>this.status=e};cx.notify();});
        }).detach();
        cx.notify();
    }
}
pub(super) fn state_label(s: State) -> &'static str {
    match s {
        State::Pending => "待同步",
        State::InFlight => "执行中",
        State::Applied => "已处理",
        State::Failed => "失败",
        State::Conflict => "冲突",
        State::Uncertain => "结果不确定",
    }
}
impl Render for Operations {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("operations-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3();
        for item in &self.items {
            let mut row = div()
                .p_4()
                .rounded_xl()
                .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                .flex()
                .flex_col()
                .gap_2()
                .child(format!(
                    "{} · {} · UID {}",
                    state_label(item.state),
                    item.location.account.0,
                    item.location.uid
                ))
                .child(match &item.operation {
                    mail_protocol::MailOperation::Flags { seen, flagged } => {
                        format!("标记 · 已读 {seen:?} · 星标 {flagged:?}")
                    }
                    mail_protocol::MailOperation::Move { .. } => "移动/归档/删除".into(),
                    _ => "删除".into(),
                });
            if let Some(d) = &item.detail {
                row = row.child(d.message.clone());
            }
            if item.state == State::Failed {
                let id = item.id.clone();
                row = row.child(
                    MaterialButton::new(format!("retry-{id}"))
                        .variant(ButtonVariant::Outlined)
                        .child("重试明确失败的操作")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load(
                                Some(Command::RetryOperation {
                                    operation: id.clone(),
                                }),
                                cx,
                            )
                        })),
                );
            }
            if matches!(
                item.state,
                State::Failed | State::Conflict | State::Uncertain
            ) {
                let id = item.id.clone();
                row = row.child(
                    MaterialButton::new(format!("reconcile-{id}"))
                        .variant(ButtonVariant::Outlined)
                        .child("只读核对远端")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load(
                                Some(Command::ResolveOperation {
                                    operation: id.clone(),
                                    resolution: OperationResolution::Reconcile,
                                }),
                                cx,
                            )
                        })),
                );
                let id = item.id.clone();
                let confirmed = self.confirm.as_ref() == Some(&id);
                row = row.child(
                    MaterialButton::new(format!("keep-{id}"))
                        .variant(ButtonVariant::Text)
                        .child(if confirmed {
                            "确认放弃本地意图，保留服务器状态"
                        } else {
                            "保留服务器状态…"
                        })
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.confirm.as_ref() == Some(&id) {
                                this.confirm = None;
                                this.load(
                                    Some(Command::ResolveOperation {
                                        operation: id.clone(),
                                        resolution: OperationResolution::KeepServer,
                                    }),
                                    cx,
                                );
                            } else {
                                this.confirm = Some(id.clone());
                                cx.notify();
                            }
                        })),
                );
            }
            list = list.child(row);
        }
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(div().text_xl().child("待同步操作"))
            .child(
                MaterialButton::new("operations-refresh")
                    .variant(ButtonVariant::Outlined)
                    .child("刷新状态")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.load(None, cx))),
            )
            .child(self.status.clone())
            .child(list)
    }
}
