CREATE TABLE IF NOT EXISTS track (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    title_sortable TEXT NOT NULL,
    album_id INTEGER,
    track_number INTEGER,
    disc_number INTEGER,
    duration INTEGER NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    genres TEXT,
    tags TEXT,
    location TEXT NOT NULL UNIQUE,
    FOREIGN KEY (album_id) REFERENCES album (id)
);
