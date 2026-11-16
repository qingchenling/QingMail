-- LIST 目录与已 EXAMINE 的文件夹分离，NoSelect 节点无需虚构 UIDVALIDITY。
CREATE TABLE remote_folders (
    account_id TEXT NOT NULL REFERENCES accounts(id),
    remote_name TEXT NOT NULL,
    display_name TEXT NOT NULL,
    delimiter TEXT,
    selectable INTEGER NOT NULL CHECK(selectable IN (0,1)),
    PRIMARY KEY(account_id,remote_name)
) STRICT;
CREATE TABLE folder_sync (
    folder_id TEXT PRIMARY KEY NOT NULL REFERENCES folders(id),
    synced_at INTEGER NOT NULL,
    since INTEGER NOT NULL,
    has_more INTEGER NOT NULL CHECK(has_more IN (0,1))
) STRICT;
CREATE TABLE catalog_sync (
    account_id TEXT PRIMARY KEY NOT NULL REFERENCES accounts(id),
    synced_at INTEGER NOT NULL
) STRICT;
