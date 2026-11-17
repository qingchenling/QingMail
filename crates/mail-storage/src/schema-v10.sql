-- 正式凭据按版本保留；只有 active 引用与连接配置同事务切换。
CREATE TABLE account_credentials (
    credential_key TEXT PRIMARY KEY NOT NULL CHECK(length(credential_key) BETWEEN 1 AND 1024),
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    active INTEGER NOT NULL CHECK(active IN (0,1))
) STRICT;
CREATE UNIQUE INDEX one_active_credential ON account_credentials(account_id) WHERE active=1;
INSERT INTO account_credentials SELECT id,id,1 FROM accounts WHERE kind='remote';
CREATE TRIGGER account_legacy_credential AFTER INSERT ON accounts WHEN NEW.kind='remote'
BEGIN
    INSERT INTO account_credentials VALUES (NEW.id,NEW.id,1);
END;
