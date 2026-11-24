CREATE TABLE public.invoice (
  id integer PRIMARY KEY,
  status text NOT NULL,
  amount numeric(20, 4) NOT NULL,
  password_hash text
);

CREATE TABLE public.line_item (
  id integer PRIMARY KEY,
  invoice_id integer NOT NULL REFERENCES public.invoice (id),
  qty integer NOT NULL
);
