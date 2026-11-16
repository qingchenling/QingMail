-- 完整MIME作为body类不可变对象；与草稿JSON由引用表区分。
CREATE TABLE message_bodies (
    account_id TEXT NOT NULL,
    folder_id TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    object_id TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'body' CHECK(kind='body'),
    PRIMARY KEY(account_id,folder_id,uidvalidity,uid),
    FOREIGN KEY(account_id,folder_id,uidvalidity,uid)
        REFERENCES folder_messages(account_id,folder_id,uidvalidity,uid) ON DELETE CASCADE,
    FOREIGN KEY(object_id,account_id,kind) REFERENCES objects(id,account_id,kind)
) STRICT;
