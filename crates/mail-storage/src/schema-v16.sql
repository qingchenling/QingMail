ALTER TABLE draft_sync ADD COLUMN lineage TEXT;
CREATE TABLE draft_scans (
 account_id TEXT PRIMARY KEY NOT NULL REFERENCES accounts(id),
 attempted_at INTEGER NOT NULL DEFAULT 0,
 detail TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO draft_scans(account_id) SELECT id FROM accounts WHERE kind='remote';
CREATE TRIGGER track_draft_scan_account AFTER INSERT ON accounts WHEN NEW.kind='remote'
BEGIN INSERT INTO draft_scans(account_id) VALUES(NEW.id); END;
CREATE TABLE draft_conflicts (
 id TEXT PRIMARY KEY NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id),
 draft_id TEXT NOT NULL,
 base_source TEXT REFERENCES server_draft_sources(id),
 remote_source TEXT REFERENCES server_draft_sources(id),
 remote_copy TEXT,
 reason TEXT NOT NULL,
 state TEXT NOT NULL DEFAULT 'open',
 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
 FOREIGN KEY(draft_id,account_id) REFERENCES drafts(id,account_id) ON DELETE CASCADE
) STRICT;
CREATE UNIQUE INDEX one_open_draft_conflict ON draft_conflicts(account_id,draft_id) WHERE state='open';
INSERT INTO draft_conflicts(id,account_id,draft_id,base_source,reason)
SELECT lower(hex(randomblob(16))),account_id,draft_id,base_source,detail FROM draft_sync WHERE state IN ('conflict','uncertain');
CREATE TRIGGER stop_sent_draft_sync AFTER UPDATE OF state ON send_tasks
WHEN NEW.state IN ('copy_pending','sent')
BEGIN
 UPDATE draft_sync SET state='stopped',detail='邮件已发送；保留服务器草稿历史，不再上传此版本'
 WHERE account_id=NEW.account_id AND draft_id=NEW.source_draft
 AND (SELECT revision FROM drafts WHERE account_id=NEW.account_id AND id=NEW.source_draft)<=NEW.source_revision;
 UPDATE draft_conflicts SET state='sent_preserved' WHERE account_id=NEW.account_id AND draft_id=NEW.source_draft AND state='open'
 AND (SELECT revision FROM drafts WHERE account_id=NEW.account_id AND id=NEW.source_draft)<=NEW.source_revision;
END;
UPDATE draft_sync SET state='stopped',detail='邮件已发送；保留服务器草稿历史，不再上传此版本'
WHERE EXISTS(SELECT 1 FROM send_tasks t JOIN drafts d ON d.id=draft_sync.draft_id AND d.account_id=draft_sync.account_id
 WHERE t.account_id=draft_sync.account_id AND t.source_draft=draft_sync.draft_id AND t.state IN ('copy_pending','sent') AND t.source_revision>=d.revision);
UPDATE draft_conflicts SET state='sent_preserved' WHERE state='open' AND EXISTS(SELECT 1 FROM draft_sync s WHERE s.account_id=draft_conflicts.account_id AND s.draft_id=draft_conflicts.draft_id AND s.state='stopped');
CREATE TABLE draft_scan_failures (
 account_id TEXT NOT NULL REFERENCES accounts(id), remote_name TEXT NOT NULL,
 uidvalidity INTEGER NOT NULL, uid INTEGER NOT NULL, attempted_at INTEGER NOT NULL DEFAULT (unixepoch()),
 detail TEXT NOT NULL, PRIMARY KEY(account_id,remote_name,uidvalidity,uid)
) STRICT;
DROP TRIGGER track_server_draft_update;
CREATE TRIGGER track_server_draft_update AFTER UPDATE OF revision ON drafts
BEGIN UPDATE draft_sync SET changed_at=unixepoch(),state=CASE
 WHEN state IN ('synced','failed') THEN 'pending'
 WHEN state='stopped' AND EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=NEW.account_id AND t.source_draft=NEW.id AND t.state IN ('copy_pending','sent') AND t.source_revision<NEW.revision) THEN 'pending'
 ELSE state END WHERE account_id=NEW.account_id AND draft_id=NEW.id; END;
