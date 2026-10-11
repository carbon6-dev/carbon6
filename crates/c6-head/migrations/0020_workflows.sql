-- Workflow runs: their steps' state (JSON array of c6_api::WorkflowStepRun).
CREATE TABLE workflow_runs (
    id           TEXT PRIMARY KEY NOT NULL,
    workflow     TEXT NOT NULL,
    input        TEXT NOT NULL,
    started_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    trigger      TEXT NOT NULL,
    status       TEXT NOT NULL,
    error        TEXT,
    steps        TEXT NOT NULL,
    -- The workflow as it was when the run started (JSON c6_api::Workflow).
    definition   TEXT NOT NULL
);
CREATE INDEX workflow_runs_by_workflow ON workflow_runs (workflow, started_at);
