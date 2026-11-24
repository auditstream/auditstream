\set id random(1, 10000)
BEGIN;
UPDATE up_t SET v = v + 1 WHERE id = :id;
COMMIT;
