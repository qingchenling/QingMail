//! 跨邮箱/阅读窗口的 M3E 控件与图标，统一尺寸、颜色和交互语义。
use gpui_kit::{ElementId, ParentElement, px};
use gpui_kit_material::{
    icons::{Icon, Symbol},
    prelude::*,
};

pub(super) fn icon(name: &str) -> Icon {
    Icon::from(Symbol::named(name).expect("pinned Material Symbols catalog")).size(px(20.))
}
pub(super) fn tool(id: impl Into<ElementId>, name: &str, label: &str) -> IconButton {
    IconButton::new(id, icon(name), label.to_owned())
        .variant(IconButtonVariant::Standard)
        .button_size(ButtonSize::Small)
}
pub(super) fn quiet(id: impl Into<ElementId>, label: &str) -> MaterialButton {
    MaterialButton::new(id)
        .variant(ButtonVariant::Text)
        .tone(ButtonTone::Neutral)
        .button_size(ButtonSize::ExtraSmall)
        .child(label.to_owned())
}

/// IMAP INBOX 是协议保留名；仅在显示层本地化，定位仍使用原名。
pub(super) fn folder_label(name: &str) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        "收件箱".into()
    } else {
        mail_service::accounts::folder_label(name)
    }
}

/// Must match the desktop entry and hicolor icon basename for Wayland grouping.
pub(super) const APP_ID: &str = "org.qingmail.QingMail";
pub(super) fn window_options() -> gpui_kit::WindowOptions {
    gpui_kit::WindowOptions {
        app_id: Some(APP_ID.into()),
        ..Default::default()
    }
}
