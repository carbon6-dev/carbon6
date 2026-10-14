-- Series (docs/os.md §4.4): numbers over time, per realm. Points carry
-- their dimensions (e.g. {"service": "invoicer"}); triggers watch series
-- for thresholds (crossed, cleared, stopped arriving).
CREATE TABLE series (
    realm           TEXT NOT NULL,
    name            TEXT NOT NULL,
    unit            TEXT NOT NULL DEFAULT '',
    description     TEXT NOT NULL DEFAULT '',
    dimensions      TEXT NOT NULL DEFAULT '[]',   -- the dimension names points may carry
    retention_days  INTEGER NOT NULL DEFAULT 0,   -- 0: kept
    system          INTEGER NOT NULL DEFAULT 0,   -- carbon6's own (host checks)
    created_by      TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (realm, name)
);
CREATE TABLE points (
    realm   TEXT NOT NULL,
    series  TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    value   REAL NOT NULL,
    dims    TEXT NOT NULL DEFAULT '{}',
    source  TEXT NOT NULL DEFAULT ''
);
CREATE INDEX points_by_series ON points (realm, series, ts);
-- Which series thresholds are crossed now (a trigger fires on the change).
CREATE TABLE series_alerts (
    trigger_id  TEXT PRIMARY KEY NOT NULL,
    crossed     INTEGER NOT NULL,
    at          INTEGER NOT NULL
);

-- Host checks keep history from now on: whether each host was up, and how
-- long the check took; the latest check of each host starts it.
INSERT INTO series (realm, name, unit, description, dimensions, retention_days, system, created_by, created_at) VALUES
    ('home', 'host_up', '', '1 when a host check reached the host, 0 when it did not.', '["host"]', 90, 1, 'system', CAST(strftime('%s', 'now') AS INTEGER) * 1000),
    ('home', 'host_check_ms', 'ms', 'How long each host check took.', '["host"]', 90, 1, 'system', CAST(strftime('%s', 'now') AS INTEGER) * 1000);
INSERT INTO points (realm, series, ts, value, dims, source)
    SELECT 'home', 'host_up', at, ok, json_object('host', name), 'host check' FROM host_checks;
INSERT INTO points (realm, series, ts, value, dims, source)
    SELECT 'home', 'host_check_ms', at, latency_ms, json_object('host', name), 'host check' FROM host_checks;
