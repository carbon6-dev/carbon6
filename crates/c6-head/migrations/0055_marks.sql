-- Marks (docs/marks.md): the colour, plain or in a pattern, that a value of
-- a session's aspects wears everywhere it shows. Keyed `<aspect>:<value>`
-- (worker:<id>, work:<id>, realm:<id>, repo:<name>, model:<name>); workers
-- and open work get one picked when first seen, the rest when you pick one.
CREATE TABLE marks (
    key TEXT PRIMARY KEY,
    mark TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
