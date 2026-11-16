//! QingMail P0 最小窗口：验证原生 Wayland 下 GPUI Kit + gpui-kit-material
//! 可编译、可运行、可渲染（主文档 §9 P0、§11 任务 2）。

use gpui_kit::prelude::*;
use gpui_kit::{
    App, Bounds, Context, IntoElement, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, application, div, px, rgb, size,
};
use gpui_kit_material::color::themes::{Contrast, ThemeId, ThemeMode};
use gpui_kit_material::icons::Assets;
use gpui_kit_material::theme;

struct Home;

impl Render for Home {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(rgb(0xfdfcff))
            .text_color(rgb(0x1c1b1f))
            .text_size(px(14.0))
            .child("QingMail · P0 原生 Wayland 窗口验证")
            .child("GPUI Kit 0.9.1 + gpui-kit-material 0.1.4 / gpui-pre 0.3.12")
    }
}

fn main() {
    application()
        .with_assets(Assets::new())
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            gpui_kit_material::init(cx);

            // 初始浅色方案；明暗切换在 P0 后续迭代接入。
            let scheme = ThemeId::Baseline.scheme(ThemeMode::Light, Contrast::Standard);
            theme::init_component_theme(scheme, 1.0, cx);

            let bounds = Bounds::centered(None, size(px(900.0), px(640.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("QingMail".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Home))
                .expect("failed to open window");
            cx.activate(true);
        });
}
