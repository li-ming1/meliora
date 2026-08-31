CREATE TABLE IF NOT EXISTS artist (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    name_sortable TEXT NOT NULL,
    bio TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    image BLOB,
    image_mime TEXT,
    tags TEXT
)
