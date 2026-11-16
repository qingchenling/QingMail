-- 远端标记缓存；按文件夹/UIDVALIDITY/UID 保存，操作成功后回写。
ALTER TABLE folder_messages ADD COLUMN seen INTEGER NOT NULL DEFAULT 0 CHECK(seen IN (0,1));
ALTER TABLE folder_messages ADD COLUMN flagged INTEGER NOT NULL DEFAULT 0 CHECK(flagged IN (0,1));
