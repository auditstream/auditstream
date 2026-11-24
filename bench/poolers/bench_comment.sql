\set id random(1, 10000)
UPDATE up_t SET v = v + 1 WHERE id = :id /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;
