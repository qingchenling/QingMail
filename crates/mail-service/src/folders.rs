//! 特殊文件夹定位：按上次记录、叶子名（modified UTF-7 解码后）确定性选择。
use mail_core::RemoteFolder;

pub const ARCHIVE_NAMES: &[&str] = &["archive", "archives", "all mail", "归档", "已归档", "封存"];
pub const ARCHIVE_PREFIXES: &[&str] = &["archive", "归档"];
pub const TRASH_NAMES: &[&str] = &[
    "trash",
    "deleted items",
    "deleted messages",
    "deleted",
    "bin",
    "垃圾箱",
    "废纸篓",
    "已删除",
    "已删除邮件",
];
pub const TRASH_PREFIXES: &[&str] = &["trash", "deleted", "已删除", "垃圾"];

/// 优先沿用上次记录的文件夹；否则按已知名称与叶子前缀选择可选节点。
pub fn choose(
    folders: &[RemoteFolder],
    preferred: Option<&str>,
    exact: &[&str],
    prefixes: &[&str],
) -> Option<String> {
    let selectable = folders.iter().filter(|folder| folder.selectable);
    if let Some(preferred) = preferred
        && let Some(folder) = selectable.clone().find(|folder| folder.name == preferred)
    {
        return Some(folder.name.clone());
    }
    let mut best: Option<(u8, &str)> = None;
    for folder in selectable {
        let leaf = leaf_name(folder);
        let score = if exact.contains(&leaf.as_str()) {
            0
        } else if prefixes.iter().any(|prefix| leaf.starts_with(prefix)) {
            1
        } else {
            continue;
        };
        if best.is_none_or(|(current, _)| score < current) {
            best = Some((score, &folder.name));
        }
    }
    best.map(|(_, name)| name.to_owned())
}

pub fn archive(folders: &[RemoteFolder], preferred: Option<&str>) -> Option<String> {
    choose(folders, preferred, ARCHIVE_NAMES, ARCHIVE_PREFIXES)
}
pub fn trash(folders: &[RemoteFolder], preferred: Option<&str>) -> Option<String> {
    choose(folders, preferred, TRASH_NAMES, TRASH_PREFIXES)
}

fn leaf_name(folder: &RemoteFolder) -> String {
    // 服务端名是 modified UTF-7；先解码，才能识别中文文件夹。
    let decoded = crate::accounts::folder_label(&folder.name);
    let leaf = match folder.delimiter.as_deref().and_then(|d| d.chars().next()) {
        Some(separator) => decoded.rsplit(separator).next().unwrap_or(&decoded),
        None => decoded.rsplit(['/', '.']).next().unwrap_or(&decoded),
    };
    leaf.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> RemoteFolder {
        RemoteFolder {
            name: name.into(),
            display_name: name.into(),
            delimiter: Some("/".into()),
            selectable: true,
        }
    }

    #[test]
    fn archive_and_trash_are_selected_without_touching_sent_names() {
        let folders = vec![
            folder("INBOX"),
            folder("Sent"),
            folder("[Gmail]/All Mail"),
            folder("[Gmail]/Trash"),
        ];
        assert_eq!(
            archive(&folders, None),
            Some("[Gmail]/All Mail".to_string())
        );
        assert_eq!(trash(&folders, None), Some("[Gmail]/Trash".to_string()));
        let chinese = vec![folder("&XfJT0ZAB-"), folder("已删除邮件")];
        assert_eq!(trash(&chinese, None), Some("已删除邮件".to_string()));
        assert_eq!(archive(&[folder("INBOX")], None), None);
        assert_eq!(archive(&folders, Some("Sent")), Some("Sent".to_string()));
    }
}
