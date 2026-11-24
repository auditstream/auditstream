\set ON_ERROR_STOP on
-- Run as the admin role directly against PostgreSQL (never through the pooler):
--   psql "$ADMIN_URL" -v app_pw="$APP_PW" -f bench/poolers/fixtures.sql
--
-- Roles are reused and never dropped: a connection pooler keeps sessions logged in as the
-- original role OID, and re-creating a role would break pooled sessions with "permission denied".

DROP SCHEMA IF EXISTS auditstream CASCADE;
DROP TABLE IF EXISTS public.line_item, public.invoice, public.up_t, public.up_plain;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'auditstream_owner') THEN
    EXECUTE 'CREATE ROLE auditstream_owner NOLOGIN';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app_user') THEN
    EXECUTE 'CREATE ROLE app_user LOGIN';
  END IF;
  IF NOT pg_catalog.pg_has_role(current_user, 'auditstream_owner', 'USAGE') THEN
    EXECUTE pg_catalog.format('GRANT auditstream_owner TO %I', current_user);
  END IF;
  IF NOT pg_catalog.pg_has_role(current_user, 'app_user', 'USAGE') THEN
    EXECUTE pg_catalog.format('GRANT app_user TO %I', current_user);
  END IF;
END
$$;

ALTER ROLE app_user LOGIN PASSWORD :'app_pw';

CREATE TABLE public.invoice (
  id int PRIMARY KEY,
  status text,
  total numeric,
  password_hash text
);

CREATE TABLE public.line_item (
  id int PRIMARY KEY,
  invoice_id int REFERENCES public.invoice(id) ON DELETE CASCADE,
  qty int
);

-- pgbench tables, seeded before capture triggers are installed so seeding is not audited.
CREATE TABLE public.up_t (
  id int PRIMARY KEY,
  v int NOT NULL DEFAULT 0
);

CREATE TABLE public.up_plain (
  id int PRIMARY KEY,
  v int NOT NULL DEFAULT 0
);

INSERT INTO public.up_t SELECT g FROM generate_series(1, 10000) AS g;
INSERT INTO public.up_plain SELECT g FROM generate_series(1, 10000) AS g;

GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON public.invoice, public.line_item TO app_user;
GRANT SELECT, UPDATE ON public.up_t, public.up_plain TO app_user;
