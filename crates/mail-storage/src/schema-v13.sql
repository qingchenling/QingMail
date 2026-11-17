CREATE TABLE message_structures (
 account_id TEXT NOT NULL REFERENCES accounts(id),
 folder_id TEXT NOT NULL,
 uidvalidity INTEGER NOT NULL,
 uid INTEGER NOT NULL,
 manifest TEXT NOT NULL,
 PRIMARY KEY(account_id,folder_id,uidvalidity,uid)
) STRICT;
CREATE TABLE part_resources (
 account_id TEXT NOT NULL,
 folder_id TEXT NOT NULL,
 uidvalidity INTEGER NOT NULL,
 uid INTEGER NOT NULL,
 section TEXT NOT NULL,
 object_id TEXT NOT NULL,
 accessed_at INTEGER NOT NULL,
 PRIMARY KEY(account_id,folder_id,uidvalidity,uid,section),
 FOREIGN KEY(account_id,folder_id,uidvalidity,uid) REFERENCES message_structures(account_id,folder_id,uidvalidity,uid),
 FOREIGN KEY(object_id,account_id) REFERENCES objects(id,account_id)
) STRICT;
