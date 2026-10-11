-- Sessions on another machine over SSH, through their worker.
ALTER TABLE sessions ADD COLUMN ssh_target TEXT NOT NULL DEFAULT '';
