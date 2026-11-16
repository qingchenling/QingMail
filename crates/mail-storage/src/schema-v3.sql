-- 未配置邮箱时使用明确的本地草稿空间，不伪装为可连接账号。
ALTER TABLE accounts ADD COLUMN kind TEXT NOT NULL DEFAULT 'remote' CHECK(kind IN ('remote','local'));
CREATE UNIQUE INDEX one_local_workspace ON accounts(kind) WHERE kind='local';
CREATE TABLE local_editor (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    draft_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    FOREIGN KEY(draft_id,account_id) REFERENCES drafts(id,account_id) ON DELETE CASCADE
) STRICT;
