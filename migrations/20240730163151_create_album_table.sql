CREATE TABLE IF NOT EXISTS album (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    title_sortable TEXT NOT NULL,
    artist_id INTEGER,
    release_date DATE,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    image BLOB,
    thumb BLOB,
    tags TEXT,
    FOREIGN KEY (artist_id) REFERENCES artist (id)
);

CREATE UNIQUE INDEX IF NOT EXISTS album_title_artist_id_idx ON album (title, artist_id);
