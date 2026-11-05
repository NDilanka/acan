ALTER TABLE events ADD COLUMN mac BLOB;

CREATE TABLE event_integrity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL CHECK (version = 1),
    next_seq INTEGER NOT NULL,
    mac BLOB NOT NULL
);
