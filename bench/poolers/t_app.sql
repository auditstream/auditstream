-- T1 sqlcommenter format with another key, Prisma-style ordering (keys sorted)
INSERT INTO invoice VALUES (1,'open',100,'s1'),(2,'open',50,'s2'),(3,'open',10,'s3') /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'*/;
INSERT INTO line_item VALUES (10,1,1),(11,1,2) /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;
-- T2 set_config wins over the comment
BEGIN; SELECT set_config('auditstream.actor', '{"type":"system","id":"nightly-job"}', true) \g /dev/null
UPDATE invoice SET status='paid' WHERE id=1 /*auditstream='eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ'*/; COMMIT;
-- T3 JWT claims fallback (Supabase / PostgREST style)
BEGIN; SELECT set_config('request.jwt.claims', '{"sub":"auth0|abc","role":"authenticated"}', true) \g /dev/null
UPDATE invoice SET total=55 WHERE id=2; COMMIT;
-- T4 spoof attempts: comment-looking text inside a string literal, not at the very end
UPDATE invoice SET status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/' WHERE id=3;
UPDATE invoice SET status='x' WHERE status='/*auditstream=''eyJ0eXBlIjoiYXBpX2tleSIsImlkIjoia2V5XzkifQ''*/';
-- T5 malformed payloads must not break the user's statement
UPDATE invoice SET total=56 WHERE id=2 /*auditstream='!!!notbase64'*/;
UPDATE invoice SET total=57 WHERE id=2 /*auditstream='bm90IGpzb24'*/;
-- T6 agent acting on behalf of a user, bulk update + cascade delete
UPDATE invoice SET status='void' WHERE id IN (2,3) /*auditstream='eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJpbGxpbmctYWdlbnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfNDIifX0'*/;
DELETE FROM invoice WHERE id=1 /*auditstream='eyJ0eXBlIjoiYWlfYWdlbnQiLCJpZCI6ImJpbGxpbmctYWdlbnQiLCJvbkJlaGFsZk9mIjp7InR5cGUiOiJ1c2VyIiwiaWQiOiJ1c3JfNDIifX0'*/;
-- T7 no-op update and excluded-column-only update
UPDATE invoice SET status=status WHERE id=2;
UPDATE invoice SET password_hash='rotated' WHERE id=2;
-- T8 TRUNCATE
TRUNCATE line_item /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;
-- T9 the application role cannot tamper with history
INSERT INTO auditstream.changes(op) VALUES ('FAKE');
UPDATE auditstream.changes SET actor = NULL;
DELETE FROM auditstream.changes;
