use super::*;
#[derive(Debug, Clone)]
pub struct AutostartIntent {
    pub revision: u64,
    pub desired: bool,
    pub pending: bool,
    pub executable: String,
}
impl Store {
    pub fn autostart_intent(&self) -> Result<AutostartIntent> {
        Ok(self.connection.query_row(
            "SELECT revision,desired,pending,executable FROM autostart_intent WHERE id=1",
            [],
            |r| {
                Ok(AutostartIntent {
                    revision: r.get::<_, i64>(0)? as u64,
                    desired: r.get(1)?,
                    pending: r.get(2)?,
                    executable: r.get(3)?,
                })
            },
        )?)
    }
    pub fn set_autostart_intent(
        &mut self,
        revision: u64,
        desired: bool,
        executable: &str,
    ) -> Result<AutostartIntent> {
        if self.connection.execute("UPDATE autostart_intent SET revision=revision+1,desired=?1,pending=1,executable=?2 WHERE id=1 AND revision=?3", params![desired,executable,i64::try_from(revision).map_err(|_|Error::Conflict)?])? != 1 { return Err(Error::Conflict); }
        self.autostart_intent()
    }
    pub fn finish_autostart_intent(&mut self, revision: u64) -> Result<()> {
        if self.connection.execute(
            "UPDATE autostart_intent SET pending=0 WHERE id=1 AND revision=?1",
            [i64::try_from(revision).map_err(|_| Error::Conflict)?],
        )? != 1
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn autostart_schema18_upgrade_preserves_settings_and_backups() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = Store::open(&data).unwrap();
        let mut prefs = store.desktop_preferences().unwrap();
        prefs.show_sender_subject = true;
        let prefs = store.update_desktop_preferences(&prefs).unwrap();
        store
            .connection
            .execute_batch("DROP TABLE autostart_intent; PRAGMA user_version=18;")
            .unwrap();
        drop(store);
        let mut store = Store::open(&data).unwrap();
        assert_eq!(store.desktop_preferences().unwrap(), prefs);
        let initial = store.autostart_intent().unwrap();
        assert!(!initial.desired && !initial.pending);
        assert!(std::fs::read_dir(&data).unwrap().any(|f| {
            f.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v18-")
        }));
        let intent = store
            .set_autostart_intent(0, true, "/usr/bin/mail-daemon")
            .unwrap();
        assert_eq!(intent.revision, 1);
        assert!(matches!(
            store.set_autostart_intent(0, false, ""),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            store.finish_autostart_intent(2),
            Err(Error::Conflict)
        ));
        drop(store);
        let mut store = Store::open(&data).unwrap();
        assert!(store.autostart_intent().unwrap().pending);
        store.finish_autostart_intent(1).unwrap();
        assert!(!store.autostart_intent().unwrap().pending);
    }
}
