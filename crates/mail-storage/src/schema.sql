CREATE TABLE accounts (
    id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    address TEXT NOT NULL,
    imap_host TEXT NOT NULL,
    imap_port INTEGER NOT NULL CHECK(imap_port BETWEEN 1 AND 65535),
    imap_username TEXT NOT NULL,
    imap_security TEXT NOT NULL CHECK(imap_security IN ('tls', 'starttls')),
    smtp_host TEXT NOT NULL,
    smtp_port INTEGER NOT NULL CHECK(smtp_port BETWEEN 1 AND 65535),
    smtp_username TEXT NOT NULL,
    smtp_security TEXT NOT NULL CHECK(smtp_security IN ('tls', 'starttls'))
) STRICT;
CREATE TABLE folders (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    remote_name TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL CHECK(uidvalidity BETWEEN 1 AND 4294967295),
    UNIQUE(account_id, remote_name),
    UNIQUE(id, account_id),
    UNIQUE(id, account_id, uidvalidity)
) STRICT;
CREATE TABLE messages (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    subject TEXT NOT NULL,
    sender TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    UNIQUE(id, account_id)
) STRICT;
-- 邮件与文件夹独立关联，允许未来显式关联多个文件夹。
-- 不按 Message-ID 合并不同账号或不同 UID 的邮件。
CREATE TABLE folder_messages (
    account_id TEXT NOT NULL,
    folder_id TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid INTEGER NOT NULL CHECK(uid BETWEEN 1 AND 4294967295),
    message_id TEXT NOT NULL,
    PRIMARY KEY(account_id, folder_id, uidvalidity, uid),
    FOREIGN KEY(folder_id, account_id, uidvalidity)
        REFERENCES folders(id, account_id, uidvalidity),
    FOREIGN KEY(message_id, account_id) REFERENCES messages(id, account_id)
) STRICT;
CREATE INDEX folder_messages_message ON folder_messages(message_id);
