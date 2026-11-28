-- Keep the accepted capability and result schema with each invocation.
CREATE TABLE project_capability_runs (
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL REFERENCES sessions(id),
    child_id TEXT NOT NULL REFERENCES sessions(id),
    project_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    contract TEXT NOT NULL,
    result TEXT,
    end_seq INTEGER,
    created_at INTEGER NOT NULL
);
CREATE INDEX project_capability_runs_parent ON project_capability_runs(parent_id);
