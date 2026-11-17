-- 只淘汰可重新下载的对象；所有用户原件及未完成任务引用保守保护。
CREATE TABLE cache_settings (id INTEGER PRIMARY KEY CHECK(id=1),budget INTEGER NOT NULL CHECK(budget BETWEEN 0 AND 1099511627776)) STRICT;
INSERT INTO cache_settings VALUES(1,1073741824);
CREATE TABLE cache_access (object_id TEXT PRIMARY KEY REFERENCES objects(id) ON DELETE CASCADE,accessed_at INTEGER NOT NULL DEFAULT(unixepoch())) STRICT;
INSERT INTO cache_access(object_id) SELECT object_id FROM message_bodies UNION SELECT object_id FROM part_resources;
CREATE TRIGGER cache_body_insert AFTER INSERT ON message_bodies BEGIN INSERT OR REPLACE INTO cache_access VALUES(NEW.object_id,unixepoch()); END;
CREATE TRIGGER cache_part_insert AFTER INSERT ON part_resources BEGIN INSERT OR REPLACE INTO cache_access VALUES(NEW.object_id,unixepoch()); END;
CREATE VIEW protected_objects AS
 SELECT body_id AS id FROM drafts UNION SELECT body_id FROM send_tasks
 UNION SELECT id FROM attachments
 UNION SELECT raw_id FROM server_draft_sources UNION SELECT raw_id FROM draft_uploads
 UNION SELECT b.object_id FROM message_bodies b JOIN mail_operations m ON m.account_id=b.account_id AND m.folder_id=b.folder_id AND m.uidvalidity=b.uidvalidity AND m.uid=b.uid WHERE m.state!='applied'
 UNION SELECT p.object_id FROM part_resources p JOIN mail_operations m ON m.account_id=p.account_id AND m.folder_id=p.folder_id AND m.uidvalidity=p.uidvalidity AND m.uid=p.uid WHERE m.state!='applied';
