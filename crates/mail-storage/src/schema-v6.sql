-- 已发送副本位置；只有 APPEND 或查重确认后才写入，不代表允许重发。
ALTER TABLE send_tasks ADD COLUMN copy_folder TEXT;
ALTER TABLE send_tasks ADD COLUMN copy_uidvalidity INTEGER CHECK(copy_uidvalidity IS NULL OR copy_uidvalidity BETWEEN 1 AND 4294967295);
ALTER TABLE send_tasks ADD COLUMN copy_uid INTEGER CHECK(copy_uid IS NULL OR copy_uid BETWEEN 1 AND 4294967295);
