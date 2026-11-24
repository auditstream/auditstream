\set id random(1, 10000)
UPDATE bench_up_plain SET v = v + 1 WHERE id = :id;
