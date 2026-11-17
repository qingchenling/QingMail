CREATE TABLE mail_operations (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 id TEXT NOT NULL UNIQUE,
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 folder_id TEXT NOT NULL,
 uidvalidity INTEGER NOT NULL,
 uid INTEGER NOT NULL,
 operation TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','in_flight','applied','failed','conflict','uncertain')),
 detail TEXT,
 attempts INTEGER NOT NULL DEFAULT 0,
 reconcile_requested INTEGER NOT NULL DEFAULT 0 CHECK(reconcile_requested IN (0,1)),
 retry_at INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX operations_account_order ON mail_operations(account_id,seq);
CREATE INDEX operations_identity ON mail_operations(account_id,folder_id,uidvalidity,uid,seq);
CREATE VIEW visible_folder_messages AS
SELECT f.account_id,f.folder_id,f.uidvalidity,f.uid,f.message_id,
 COALESCE((SELECT json_extract(o.operation,'$.seen') FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.seen') IS NOT NULL ORDER BY seq DESC LIMIT 1),f.seen) AS seen,
 COALESCE((SELECT json_extract(o.operation,'$.flagged') FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.flagged') IS NOT NULL ORDER BY seq DESC LIMIT 1),f.flagged) AS flagged
FROM folder_messages f WHERE NOT EXISTS(SELECT 1 FROM mail_operations o WHERE o.account_id=f.account_id AND o.folder_id=f.folder_id AND o.uidvalidity=f.uidvalidity AND o.uid=f.uid AND o.state!='applied' AND json_extract(o.operation,'$.type') IN ('move','delete'));
