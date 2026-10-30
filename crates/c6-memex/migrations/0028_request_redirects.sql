-- A saved request reference outlives a merge. Each realm's knowledge store
-- keeps its own redirects; retaining every edge also keeps the merge history.
CREATE TABLE request_redirects (
    from_id    INTEGER PRIMARY KEY NOT NULL,
    into_id    INTEGER NOT NULL,
    merged_at  INTEGER NOT NULL,
    CHECK (from_id != into_id)
);
CREATE INDEX request_redirects_into ON request_redirects (into_id);
