-- IPC 请求先记录执行权；崩溃后未确认完成的请求不得自动再次执行。
CREATE TABLE mutation_results (
    request_id TEXT PRIMARY KEY NOT NULL CHECK(length(request_id)=32),
    fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
    state TEXT NOT NULL CHECK(state IN ('in_progress','uncertain','finished')),
    result TEXT,
    created_at INTEGER NOT NULL,
    CHECK((state='finished' AND result IS NOT NULL) OR (state!='finished' AND result IS NULL))
) STRICT;
