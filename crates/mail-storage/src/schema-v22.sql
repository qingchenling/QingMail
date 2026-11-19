-- Keep cloud recovery evidence after withdrawing a draft from the editor.
ALTER TABLE drafts ADD COLUMN deleted_at INTEGER CHECK(deleted_at IS NULL OR deleted_at >= 0);
ALTER TABLE draft_sync ADD COLUMN local_only INTEGER NOT NULL DEFAULT 0 CHECK(local_only IN (0,1));
DROP TRIGGER track_server_draft_update;
CREATE TRIGGER track_server_draft_update AFTER UPDATE OF revision ON drafts
BEGIN UPDATE draft_sync SET changed_at=unixepoch(),state=CASE
 WHEN local_only=1 THEN 'stopped'
 WHEN state IN ('synced','failed') THEN 'pending'
 WHEN state='stopped' AND EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=NEW.account_id AND t.source_draft=NEW.id AND t.state IN ('copy_pending','sent') AND t.source_revision<NEW.revision) THEN 'pending'
 ELSE state END WHERE account_id=NEW.account_id AND draft_id=NEW.id; END;
