//! 发送任务入口：跨草稿查看状态与原因，受控重发、停止处理、仅重试副本；不自动触发网络。
use gpui_kit::{
    App, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, SendState};
use mail_daemon::storage::{StorageClient, receive};
use mail_storage::SendTask;

pub(super) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_send_tasks(cx) {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(980.), px(720.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 发送任务".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SendTasks::new(window, cx))
    }) {
        Ok((window, _)) => crate::lifecycle::register_send_tasks(window, cx),
        Err(e) => eprintln!("无法打开发送任务窗口：{e}"),
    }
}

type TaskReply = mail_daemon::storage::StorageResult<
    std::sync::mpsc::Receiver<mail_daemon::storage::StorageResult<SendTask>>,
>;

pub(super) struct SendTasks {
    store: StorageClient,
    account: Option<AccountId>,
    address: String,
    tasks: Vec<SendTask>,
    status: String,
    busy: bool,
    generation: u64,
    confirming: Option<mail_core::SendTaskId>,
}

impl SendTasks {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            account: None,
            address: String::new(),
            tasks: vec![],
            status: "正在读取发送任务…".into(),
            busy: false,
            generation: 0,
            confirming: None,
        };
        this.reload(cx, false);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>, keep_status: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.generation += 1;
        let generation = self.generation;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let Some((account, settings)) = accounts.into_iter().next() else {
                return Ok::<_, String>((None, String::new(), vec![]));
            };
            let tasks = receive(store.tasks(account.clone()))?;
            Ok((Some(account), settings.address, tasks))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.busy = false;
                match result {
                    Ok((account, address, tasks)) => {
                        this.account = account;
                        this.address = address;
                        this.tasks = tasks;
                        if this.account.is_none() {
                            this.status = "尚未添加邮箱".into();
                        } else if this.tasks.is_empty() {
                            this.status = "还没有发送任务".into();
                        } else if !keep_status {
                            this.status = format!(
                                "共 {} 条任务；重发与停止只在明确确认后执行",
                                this.tasks.len()
                            );
                        }
                    }
                    Err(e) => this.status = format!("发送任务读取失败：{e}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn run_reply(&mut self, cx: &mut Context<Self>, pending: &str, reply: TaskReply) {
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                self.status = e;
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.status = pending.into();
        let task = cx.background_executor().spawn(async move {
            reply
                .recv()
                .map_err(|_| "发送任务中断，请刷新查看结果".to_owned())
                .and_then(|r| r)
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.status = match result {
                    Ok(task) => crate::send_labels::state_label(&task),
                    Err(e) => format!("未完成：{e}"),
                };
                this.reload(cx, true);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn retry_copy(&mut self, task: &SendTask, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let reply = crate::lifecycle::sending(cx).retry_copy(
            self.store.clone(),
            task.account.clone(),
            task.id.clone(),
        );
        self.run_reply(cx, "正在重试保存已发送副本；不会重发邮件…", reply);
    }

    fn resend(&mut self, task: &SendTask, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.confirming = None;
        let reply = crate::lifecycle::sending(cx).resend(
            self.store.clone(),
            task.account.clone(),
            task.id.clone(),
        );
        self.run_reply(cx, "正在按冻结内容重发；关闭窗口不会取消…", reply);
    }

    fn stop(&mut self, task: &SendTask, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let reply = self
            .store
            .stop_uncertain(task.account.clone(), task.id.clone());
        self.run_reply(cx, "正在标记停止处理…", reply);
    }

    fn row(&self, task: &SendTask, cx: &mut Context<Self>) -> impl IntoElement {
        let label = crate::send_labels::state_label(task);
        let subject = if task.recipients.subject.trim().is_empty() {
            "（无主题）"
        } else {
            task.recipients.subject.as_str()
        };
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER))
            .child(
                div()
                    .text_size(px(14.))
                    .child(format!("{subject}  ·  {label}")),
            )
            .child(div().text_size(px(12.)).child(format!(
                "任务 {} · 草稿 {} 版本 {} · {}",
                short_id(&task.id.0),
                short_id(&task.source_draft.0),
                task.source_revision,
                if task.resend_of.is_some() {
                    "重发任务"
                } else {
                    "首次提交"
                }
            )))
            .child(
                div()
                    .text_size(px(12.))
                    .child(format!("发件人：{}", task.sender)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .child(format!("收件人：{}", recipients(task))),
            )
            .child(div().text_size(px(12.)).child(format!(
                "时间：{}",
                mail_service::sync::display_time(task.created_at)
            )));
        if let Some(copy) = &task.copy {
            card = card.child(div().text_size(px(12.)).child(format!(
                "副本：{} · UIDVALIDITY {} · UID {}",
                copy.folder,
                copy.uidvalidity,
                copy.uid
                    .map(|uid| uid.to_string())
                    .unwrap_or_else(|| "未知".into())
            )));
        }
        let mut actions = div().flex().gap_2().flex_wrap();
        if task.state == SendState::CopyPending {
            let task = task.clone();
            actions = actions.child(
                MaterialButton::new(format!("task-retry-copy-{}", task.id.0))
                    .child("重试保存副本")
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.retry_copy(&task, cx))),
            );
        }
        if matches!(task.state, SendState::Failed | SendState::Uncertain) {
            let task = task.clone();
            actions = actions.child(
                MaterialButton::new(format!("task-resend-{}", task.id.0))
                    .child(if task.state == SendState::Uncertain {
                        "重发（可能重复）…"
                    } else {
                        "重发…"
                    })
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirming = Some(task.id.clone());
                        cx.notify();
                    })),
            );
        }
        if task.state == SendState::Uncertain && task.detail != "user_stopped_processing" {
            let task = task.clone();
            actions = actions.child(
                MaterialButton::new(format!("task-stop-{}", task.id.0))
                    .child("停止处理")
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.stop(&task, cx))),
            );
        }
        card = card.child(actions);
        if self.confirming.as_ref() == Some(&task.id) {
            let warning = if task.state == SendState::Uncertain {
                "原任务结果不确定，收件人可能已经收到；确认后会以新邮件再次提交，可能产生重复。旧任务记录会保留。"
            } else {
                "确认后会按原冻结内容再次提交；旧任务记录会保留，不会自动重发。"
            };
            let confirmed = task.clone();
            let cancelled = task.id.clone();
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .child(div().text_size(px(12.)).child(warning))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                MaterialButton::new(format!("task-confirm-resend-{}", task.id.0))
                                    .child("确认重发")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.resend(&confirmed, cx);
                                    })),
                            )
                            .child(
                                MaterialButton::new(format!("task-cancel-resend-{}", task.id.0))
                                    .child("取消")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.confirming.as_ref() == Some(&cancelled) {
                                            this.confirming = None;
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            );
        }
        card
    }
}

impl Render for SendTasks {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .size_full()
            .bg(theme::color(ColorRole::SURFACE));
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_size(px(18.)).child("发送任务"))
                .child(
                    MaterialButton::new("send-tasks-refresh")
                        .child("刷新")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx, false))),
                ),
        );
        if !self.address.is_empty() {
            column = column.child(
                div()
                    .text_size(px(13.))
                    .child(format!("账号：{}", self.address)),
            );
        }
        column = column.child(div().text_size(px(13.)).child(self.status.clone()));
        let mut list = div()
            .id("send-task-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        for task in &self.tasks {
            list = list.child(self.row(task, cx));
        }
        column.child(list)
    }
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}
fn recipients(task: &SendTask) -> String {
    let mut parts = task.recipients.to.clone();
    if !task.recipients.cc.is_empty() {
        parts.push(format!("抄送：{}", task.recipients.cc.join(", ")));
    }
    if !task.recipients.bcc.is_empty() {
        parts.push(format!("密送：{}", task.recipients.bcc.join(", ")));
    }
    if parts.is_empty() {
        "（无）".into()
    } else {
        parts.join("；")
    }
}
