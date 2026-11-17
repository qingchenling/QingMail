-- 原件只追加；转换不会更新或删除远端及原始字节。
CREATE TABLE server_draft_folders (
 account_id TEXT PRIMARY KEY NOT NULL REFERENCES accounts(id),
 remote_name TEXT NOT NULL,
 explicit INTEGER NOT NULL CHECK(explicit IN (0,1))
) STRICT;
CREATE TABLE server_draft_sources (
 id TEXT PRIMARY KEY NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id),
 remote_name TEXT NOT NULL,
 uidvalidity INTEGER NOT NULL CHECK(uidvalidity BETWEEN 1 AND 4294967295),
 uid INTEGER NOT NULL CHECK(uid BETWEEN 1 AND 4294967295),
 raw_id TEXT NOT NULL,
 raw_hash TEXT NOT NULL,
 subject TEXT NOT NULL,
 converted TEXT,
 baseline_revision INTEGER,
 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
 UNIQUE(account_id,remote_name,uidvalidity,uid,raw_hash),
 FOREIGN KEY(raw_id,account_id) REFERENCES objects(id,account_id)
) STRICT;
CREATE INDEX server_draft_source_account ON server_draft_sources(account_id,created_at DESC);
