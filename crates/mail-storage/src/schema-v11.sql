CREATE TABLE account_options (
    account_id TEXT PRIMARY KEY NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    is_default INTEGER NOT NULL DEFAULT 0 CHECK(is_default IN (0,1)),
    paused INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0,1))
) STRICT;
CREATE UNIQUE INDEX one_default_account ON account_options(is_default) WHERE is_default=1;

INSERT INTO account_options(account_id,is_default)
SELECT id, id=(SELECT id FROM accounts WHERE kind='remote' ORDER BY id LIMIT 1)
FROM accounts WHERE kind='remote';
CREATE TRIGGER account_default_options AFTER INSERT ON accounts WHEN NEW.kind='remote'
BEGIN
    INSERT INTO account_options(account_id,is_default)
    VALUES (NEW.id,NOT EXISTS(SELECT 1 FROM account_options WHERE is_default=1));
END;
