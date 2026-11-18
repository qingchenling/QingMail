-- Intent is durable before touching XDG. Actual state is always inspected separately.
CREATE TABLE autostart_intent (
 id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL DEFAULT 0,
 desired INTEGER NOT NULL DEFAULT 0 CHECK(desired IN (0,1)),
 pending INTEGER NOT NULL DEFAULT 0 CHECK(pending IN (0,1)),
 executable TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO autostart_intent(id) VALUES(1);
