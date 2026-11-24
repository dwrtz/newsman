CREATE TABLE IF NOT EXISTS newsman_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
);
INSERT OR IGNORE INTO newsman_state(id, revision) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS newsman_records (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    body TEXT NOT NULL CHECK (json_valid(body)),
    PRIMARY KEY (kind, id)
) WITHOUT ROWID;
