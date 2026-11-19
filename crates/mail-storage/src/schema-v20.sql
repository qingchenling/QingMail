-- 新发送默认不再另存服务器副本；已经确认的任务继续沿用原来的策略。
ALTER TABLE desktop_settings ADD COLUMN save_sent_copy INTEGER NOT NULL DEFAULT 0 CHECK(save_sent_copy IN (0,1));
ALTER TABLE send_tasks ADD COLUMN save_sent_copy INTEGER NOT NULL DEFAULT 1 CHECK(save_sent_copy IN (0,1));

-- 冻结垃圾箱名称，不为未打开的目标目录虚构 UIDVALIDITY。
DROP VIEW visible_folder_messages;
CREATE VIEW visible_folder_messages AS
SELECT f.account_id,f.folder_id,f.uidvalidity,f.uid,f.message_id,
 COALESCE((SELECT json_extract(o.operation,'$.seen') FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.seen') IS NOT NULL ORDER BY seq DESC LIMIT 1),f.seen) AS seen,
 COALESCE((SELECT json_extract(o.operation,'$.flagged') FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.flagged') IS NOT NULL ORDER BY seq DESC LIMIT 1),f.flagged) AS flagged
FROM folder_messages f WHERE NOT EXISTS(SELECT 1 FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.type') IN ('move','delete','trash'));
