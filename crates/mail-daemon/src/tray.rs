//! 最小 StatusNotifierItem + DBusMenu。可用性由 watcher/host 决定，不伪装注册成功。
use crate::Command;
use std::{collections::HashMap, sync::mpsc::SyncSender};
use zbus::{
    blocking::{Connection, Proxy, connection::Builder},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

pub struct Tray {
    _connection: Connection,
}
struct Item {
    queue: SyncSender<Command>,
}
#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.queue.try_send(Command::Open);
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.queue.try_send(Command::Open);
    }
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: &str) {}
    #[zbus(property)]
    fn category(&self) -> &str {
        "Communications"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "qingmail-p0"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        "QingMail · P0 合成后台"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "mail-unread"
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let mut bytes = vec![0; 24 * 24 * 4];
        for y in 5..19 {
            for x in 2..22 {
                let edge =
                    y == 5 || y == 18 || x == 2 || x == 21 || y == 5 + (x - 2).min(21 - x) / 2;
                if edge {
                    bytes[(y * 24 + x) * 4..(y * 24 + x) * 4 + 4]
                        .copy_from_slice(&[255, 110, 90, 180]);
                }
            }
        }
        vec![(24, 24, bytes)]
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/Menu").unwrap()
    }
}
struct Menu {
    queue: SyncSender<Command>,
}
type Properties = HashMap<String, Value<'static>>;
type Layout = (i32, Properties, Vec<Value<'static>>);
fn properties(id: i32) -> Properties {
    let mut props = HashMap::new();
    if id == 0 {
        props.insert("children-display".into(), Value::from("submenu"));
    } else {
        props.insert(
            "label".into(),
            Value::from(if id == 1 {
                "打开 QingMail"
            } else {
                "保存并退出"
            }),
        );
        props.insert("enabled".into(), Value::from(true));
        props.insert("visible".into(), Value::from(true));
    }
    props
}
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, Layout) {
        let children = if parent_id == 0 && recursion_depth != 0 {
            (1..=2)
                .map(|id| Value::from((id, properties(id), Vec::<Value<'static>>::new())))
                .collect()
        } else {
            vec![]
        };
        (1, (parent_id, properties(parent_id), children))
    }
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        ids.into_iter().map(|id| (id, properties(id))).collect()
    }
    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<Value<'static>> {
        properties(id)
            .remove(name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown property".into()))
    }
    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
        if event_id == "clicked" {
            if id == 1 {
                let _ = self.queue.try_send(Command::Open);
            } else if id == 2 {
                let _ = self.queue.try_send(Command::Quit);
            }
        }
    }
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        vec![]
    }
}
impl Tray {
    /// 在后台线程调用。没有宿主时保持可恢复入口（再次启动/命令行），界面明确说明。
    pub fn start(queue: SyncSender<Command>) -> Result<Self, String> {
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let connection = Builder::session()
            .map_err(|e| e.to_string())?
            .method_timeout(std::time::Duration::from_secs(2))
            .name(name.as_str())
            .map_err(|e| e.to_string())?
            .serve_at(
                "/StatusNotifierItem",
                Item {
                    queue: queue.clone(),
                },
            )
            .map_err(|e| e.to_string())?
            .serve_at("/Menu", Menu { queue })
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        let watcher = Proxy::new(
            &connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .map_err(|e| e.to_string())?;
        let host: bool = watcher
            .get_property("IsStatusNotifierHostRegistered")
            .map_err(|e| e.to_string())?;
        if !host {
            return Err("桌面未提供托盘宿主".into());
        }
        watcher
            .call::<_, _, ()>("RegisterStatusNotifierItem", &(name.as_str(),))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            _connection: connection,
        })
    }
}
