-- Session labels: the ones you attach (JSON array), and what the automatic
-- ones are made of (worker name, git remote, folder, home folder).
ALTER TABLE sessions ADD COLUMN labels TEXT NOT NULL DEFAULT '[]';
ALTER TABLE sessions ADD COLUMN worker_name TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN repo TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN folder TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN home TEXT NOT NULL DEFAULT '';
UPDATE sessions SET worker_name = COALESCE((SELECT name FROM workers WHERE workers.id = sessions.worker_id), '');
UPDATE sessions SET folder = cwd WHERE cwd LIKE '/%';
