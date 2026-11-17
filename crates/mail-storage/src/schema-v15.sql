CREATE TABLE draft_sync (
 account_id TEXT NOT NULL,
 draft_id TEXT NOT NULL,
 base_source TEXT,
 base_revision INTEGER NOT NULL DEFAULT 0,
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','synced','failed','conflict','uncertain','stopped')),
 detail TEXT NOT NULL DEFAULT '',
 changed_at INTEGER NOT NULL DEFAULT (unixepoch()),
 attempted_at INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account_id,draft_id),
 FOREIGN KEY(draft_id,account_id) REFERENCES drafts(id,account_id) ON DELETE CASCADE,
 FOREIGN KEY(base_source) REFERENCES server_draft_sources(id)
) STRICT;
CREATE TABLE draft_uploads (
 id TEXT PRIMARY KEY NOT NULL,
 account_id TEXT NOT NULL,
 draft_id TEXT NOT NULL,
 revision INTEGER NOT NULL,
 remote_name TEXT NOT NULL,
 expected_epoch INTEGER,
 raw_id TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('prepared','in_flight','uncertain','applied','failed','conflict')),
 detail TEXT NOT NULL DEFAULT '',
 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
 UNIQUE(account_id,draft_id,revision),
 FOREIGN KEY(draft_id,account_id) REFERENCES drafts(id,account_id) ON DELETE CASCADE,
 FOREIGN KEY(raw_id,account_id) REFERENCES objects(id,account_id)
) STRICT;
INSERT INTO draft_sync(account_id,draft_id,base_source,base_revision,state)
SELECT d.account_id,d.id,s.id,coalesce(s.baseline_revision,0),CASE WHEN d.revision=coalesce(s.baseline_revision,0) THEN 'synced' ELSE 'pending' END
FROM drafts d JOIN accounts a ON a.id=d.account_id AND a.kind='remote'
LEFT JOIN server_draft_sources s ON s.id=(SELECT id FROM server_draft_sources WHERE account_id=d.account_id AND converted=d.id ORDER BY created_at DESC,rowid DESC LIMIT 1);
CREATE TRIGGER track_server_draft_insert AFTER INSERT ON drafts
WHEN EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND kind='remote')
BEGIN INSERT INTO draft_sync(account_id,draft_id) VALUES(NEW.account_id,NEW.id); END;
CREATE TRIGGER track_server_draft_update AFTER UPDATE OF revision ON drafts
BEGIN UPDATE draft_sync SET changed_at=unixepoch(),state=CASE WHEN state IN ('synced','failed') THEN 'pending' ELSE state END WHERE account_id=NEW.account_id AND draft_id=NEW.id; END;
