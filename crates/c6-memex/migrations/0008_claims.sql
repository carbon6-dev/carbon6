-- Claims (docs/apps.md §9): every link is a claim with an owner, who alone
-- retracts it; anyone may add their own. The graph stores claims and does
-- not judge between them: readers choose whose to believe.
--
-- The owner is the principal behind made_by: you; an app instance
-- (app:<name>); the realm's own agents (realm); or the program that made
-- it (gmail, a pipeline).
ALTER TABLE links ADD COLUMN owner TEXT NOT NULL DEFAULT '';
UPDATE links SET owner = CASE
    WHEN made_by LIKE 'session:%' THEN 'realm'
    WHEN made_by LIKE 'app:%' THEN substr(made_by, 1, instr(made_by || ' ', ' ') - 1)
    ELSE made_by END;
-- Writers that don't name an owner get the one their made_by implies.
CREATE TRIGGER links_owner AFTER INSERT ON links WHEN NEW.owner = '' BEGIN
    UPDATE links SET owner = CASE
        WHEN NEW.made_by LIKE 'session:%' THEN 'realm'
        WHEN NEW.made_by LIKE 'app:%' THEN substr(NEW.made_by, 1, instr(NEW.made_by || ' ', ' ') - 1)
        ELSE NEW.made_by END
    WHERE id = NEW.id;
END;
CREATE INDEX links_owner ON links (owner, created_at);
CREATE INDEX links_rel ON links (rel);

-- Retracted claims: kept, out of the graph.
CREATE TABLE retracted_links (
    id            INTEGER PRIMARY KEY,
    src_thing     TEXT,
    src_item      TEXT,
    rel           TEXT NOT NULL,
    dst_thing     TEXT,
    dst_item      TEXT,
    basis         TEXT NOT NULL,
    confidence    REAL NOT NULL,
    made_by       TEXT NOT NULL,
    owner         TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    retracted_by  TEXT NOT NULL,
    retracted_at  INTEGER NOT NULL
);

-- Facts are claims too: their owner.
ALTER TABLE facts ADD COLUMN owner TEXT NOT NULL DEFAULT '';
UPDATE facts SET owner = CASE
    WHEN made_by LIKE 'session:%' THEN 'realm'
    WHEN made_by LIKE 'app:%' THEN substr(made_by, 1, instr(made_by || ' ', ' ') - 1)
    ELSE made_by END;
CREATE TRIGGER facts_owner AFTER INSERT ON facts WHEN NEW.owner = '' BEGIN
    UPDATE facts SET owner = CASE
        WHEN NEW.made_by LIKE 'session:%' THEN 'realm'
        WHEN NEW.made_by LIKE 'app:%' THEN substr(NEW.made_by, 1, instr(NEW.made_by || ' ', ' ') - 1)
        ELSE NEW.made_by END
    WHERE id = NEW.id;
END;
