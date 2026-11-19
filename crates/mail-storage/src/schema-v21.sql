-- Existing generic accounts retain password authentication; only explicit Microsoft login selects OAuth.
ALTER TABLE accounts ADD COLUMN imap_auth TEXT NOT NULL DEFAULT 'password' CHECK(imap_auth IN ('password','microsoft_personal','microsoft365'));
ALTER TABLE accounts ADD COLUMN smtp_auth TEXT NOT NULL DEFAULT 'password' CHECK(smtp_auth IN ('password','microsoft_personal','microsoft365'));
