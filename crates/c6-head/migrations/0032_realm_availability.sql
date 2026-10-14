-- Workers and model connections are shared infrastructure (docs/os.md
-- §3.2): each is available to every realm (an empty list), or to the
-- realms it names (one: dedicated to it).
ALTER TABLE workers ADD COLUMN realms TEXT NOT NULL DEFAULT '[]';
ALTER TABLE connections ADD COLUMN realms TEXT NOT NULL DEFAULT '[]';
