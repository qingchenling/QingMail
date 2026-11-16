use std::{env, path::PathBuf};

use crate::{Error, Result};

/// 配置/数据/缓存分别解析；运行目录不回退到共享 /tmp。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub runtime: Option<PathBuf>,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        Self::from_lookup(|name| env::var_os(name).map(PathBuf::from))
    }

    fn from_lookup(get: impl Fn(&str) -> Option<PathBuf>) -> Result<Self> {
        let home = get("HOME").filter(|p| p.is_absolute());
        let resolve = |key, fallback| {
            get(key)
                .filter(|p| p.is_absolute())
                .or_else(|| home.as_ref().map(|h| h.join(fallback)))
                .map(|p| p.join("qingmail"))
                .ok_or(Error::Invalid("缺少绝对 HOME 或 XDG 目录"))
        };
        Ok(Self {
            config: resolve("XDG_CONFIG_HOME", ".config")?,
            data: resolve("XDG_DATA_HOME", ".local/share")?,
            cache: resolve("XDG_CACHE_HOME", ".cache")?,
            runtime: get("XDG_RUNTIME_DIR")
                .filter(|p| p.is_absolute())
                .map(|p| p.join("qingmail")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_absolute_overrides_relative_fallback_and_no_runtime_fallback() {
        let paths = AppPaths::from_lookup(|name| match name {
            "HOME" => Some("/home/tester".into()),
            "XDG_DATA_HOME" => Some("/data/mail".into()),
            "XDG_CONFIG_HOME" => Some("relative/ignored".into()),
            "XDG_RUNTIME_DIR" => Some("relative/ignored".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(paths.data, PathBuf::from("/data/mail/qingmail"));
        assert_eq!(paths.config, PathBuf::from("/home/tester/.config/qingmail"));
        assert_eq!(paths.cache, PathBuf::from("/home/tester/.cache/qingmail"));
        assert_eq!(paths.runtime, None);
        assert!(AppPaths::from_lookup(|_| None).is_err());
    }
}
