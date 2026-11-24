\set id random(1, 10000)
UPDATE bench_up_cap SET v = v + 1 WHERE id = :id /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidmlhX2NvbW1lbnQifQ'*/;
