-- A session's background tasks while they run (docs/background.md §4):
-- `counted` ones keep it showing as running; the others (servers and
-- watchers started with notify: false) are only listed.
CREATE TABLE running_tasks (
    session_id TEXT NOT NULL,
    task_id    TEXT NOT NULL,
    counted    INTEGER NOT NULL,
    PRIMARY KEY (session_id, task_id)
) WITHOUT ROWID;
