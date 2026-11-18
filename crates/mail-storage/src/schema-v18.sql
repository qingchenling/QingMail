-- 后台桌面偏好与通知区间：每文件夹常数空间，不保存正文或秘密。
CREATE TABLE desktop_settings (
 id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL DEFAULT 0,
 notifications INTEGER NOT NULL DEFAULT 1 CHECK(notifications IN (0,1)),
 show_sender_subject INTEGER NOT NULL DEFAULT 0 CHECK(show_sender_subject IN (0,1)),
 show_preview INTEGER NOT NULL DEFAULT 0 CHECK(show_preview IN (0,1))
) STRICT;
INSERT INTO desktop_settings(id) VALUES(1);
CREATE TABLE notification_baselines (
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
 uidvalidity INTEGER NOT NULL, high_uid INTEGER NOT NULL,
 next_attempt INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account_id,folder_id)
) STRICT;
CREATE TABLE notification_pending (
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
 uidvalidity INTEGER NOT NULL, low_uid INTEGER NOT NULL, high_uid INTEGER NOT NULL,
 created_at INTEGER NOT NULL,
 PRIMARY KEY(account_id,folder_id)
) STRICT;
-- 仅保留尚待合并的未读候选UID，避免“新到达时已读、之后改未读”误报。
-- 不复制头部或正文；发送/抑制后立即删除，不作为永久通知历史。
CREATE TABLE notification_candidates (
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
 uidvalidity INTEGER NOT NULL, uid INTEGER NOT NULL,
 PRIMARY KEY(account_id,folder_id,uidvalidity,uid)
) STRICT;
