-- Decisions (docs/os.md §4.2): every time carbon6 needs you. Action items
-- become decisions in place, keeping their ids ("action item #12" is
-- decision 12); tool-call approvals, workflow approve steps and the
-- assistant's questions open decisions too, linked by `ref` to what waits.
ALTER TABLE todos RENAME TO decisions;
ALTER TABLE decisions RENAME COLUMN detail TO document;
ALTER TABLE decisions RENAME COLUMN resolution TO note;
ALTER TABLE decisions ADD COLUMN options TEXT NOT NULL DEFAULT '[]';     -- [{label, does}]
ALTER TABLE decisions ADD COLUMN form TEXT NOT NULL DEFAULT '[]';        -- fields to fill in
ALTER TABLE decisions ADD COLUMN deadline INTEGER;
ALTER TABLE decisions ADD COLUMN default_answer TEXT;                    -- the option taken at the deadline
ALTER TABLE decisions ADD COLUMN answer TEXT;                            -- {"option": …, "values": {…}}
ALTER TABLE decisions ADD COLUMN answered_via TEXT NOT NULL DEFAULT '';
ALTER TABLE decisions ADD COLUMN answered_at INTEGER;
ALTER TABLE decisions ADD COLUMN ref TEXT NOT NULL DEFAULT '';           -- approval:/workflow:/question:/session:
UPDATE decisions SET state = 'answered', answered_at = updated_at, answered_via = 'action item' WHERE state = 'done';
CREATE INDEX decisions_ref ON decisions (ref);
CREATE INDEX decisions_open ON decisions (realm, state, updated_at);
