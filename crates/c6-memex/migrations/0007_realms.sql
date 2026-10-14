-- Realms (docs/os.md §3): the memex's data belongs to realms. Everything
-- that exists is home's.
ALTER TABLE items ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE things ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE links ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE trails ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE facts ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE commitments ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE review ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE files ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE mail ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE outgoing ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE calendars ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE contacts ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE media ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
CREATE INDEX items_realm ON items (realm, kind);
