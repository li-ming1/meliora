CREATE TABLE listen_event (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    track_key TEXT NOT NULL,
    seconds INTEGER NOT NULL CHECK (seconds > 0)
);

CREATE INDEX listen_event_ts_idx ON listen_event (ts);
CREATE INDEX listen_event_track_idx ON listen_event (track_key);

CREATE TABLE listen_track_meta (
    track_key TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    artist TEXT NOT NULL DEFAULT '',
    album TEXT NOT NULL DEFAULT ''
);
