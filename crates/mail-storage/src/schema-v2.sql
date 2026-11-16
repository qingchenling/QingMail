CREATE TABLE objects (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    kind TEXT NOT NULL CHECK(kind IN ('body','attachment')),
    size INTEGER NOT NULL CHECK(size >= 0),
    sha256 TEXT NOT NULL CHECK(length(sha256)=64),
    UNIQUE(id, account_id),
    UNIQUE(id, account_id, kind)
) STRICT;
CREATE TABLE attachments (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'attachment' CHECK(kind='attachment'),
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    UNIQUE(id, account_id),
    FOREIGN KEY(id, account_id, kind) REFERENCES objects(id, account_id, kind)
) STRICT;
CREATE TABLE drafts (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    revision INTEGER NOT NULL CHECK(revision BETWEEN 1 AND 4294967295),
    subject TEXT NOT NULL,
    recipients TEXT NOT NULL,
    body_id TEXT NOT NULL,
    body_kind TEXT NOT NULL DEFAULT 'body' CHECK(body_kind='body'),
    UNIQUE(id, account_id),
    FOREIGN KEY(body_id, account_id, body_kind) REFERENCES objects(id, account_id, kind)
) STRICT;
CREATE TABLE draft_attachments (
    draft_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    attachment_id TEXT NOT NULL,
    PRIMARY KEY(draft_id, ordinal),
    UNIQUE(draft_id, attachment_id),
    FOREIGN KEY(draft_id, account_id) REFERENCES drafts(id, account_id) ON DELETE CASCADE,
    FOREIGN KEY(attachment_id, account_id) REFERENCES attachments(id, account_id)
) STRICT;
CREATE TABLE send_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    source_draft TEXT NOT NULL,
    source_revision INTEGER NOT NULL CHECK(source_revision BETWEEN 1 AND 4294967295),
    sender TEXT NOT NULL,
    subject TEXT NOT NULL,
    recipients TEXT NOT NULL,
    body_id TEXT NOT NULL,
    body_kind TEXT NOT NULL DEFAULT 'body' CHECK(body_kind='body'),
    state TEXT NOT NULL CHECK(state IN ('pending','sending','sent','failed','uncertain','copy_pending')),
    detail TEXT NOT NULL DEFAULT '',
    resend_of TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE(id, account_id),
    UNIQUE(resend_of),
    FOREIGN KEY(body_id, account_id, body_kind) REFERENCES objects(id, account_id, kind),
    FOREIGN KEY(resend_of, account_id) REFERENCES send_tasks(id, account_id)
) STRICT;
CREATE UNIQUE INDEX initial_submission ON send_tasks(source_draft, source_revision) WHERE resend_of IS NULL;
CREATE TABLE task_attachments (
    task_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    attachment_id TEXT NOT NULL,
    PRIMARY KEY(task_id, ordinal),
    UNIQUE(task_id, attachment_id),
    FOREIGN KEY(task_id, account_id) REFERENCES send_tasks(id, account_id),
    FOREIGN KEY(attachment_id, account_id) REFERENCES attachments(id, account_id)
) STRICT;
CREATE INDEX drafts_account ON drafts(account_id);
CREATE INDEX send_tasks_account ON send_tasks(account_id);
