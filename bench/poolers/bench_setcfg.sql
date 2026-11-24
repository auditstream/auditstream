\set id random(1, 10000)
BEGIN;
SELECT set_config('auditstream.actor', '{"type":"user","id":"via_set_config"}', true);
UPDATE up_t SET v = v + 1 WHERE id = :id;
COMMIT;
