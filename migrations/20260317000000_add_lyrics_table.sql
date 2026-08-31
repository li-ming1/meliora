CREATE TABLE lyrics (
    track_id INTEGER PRIMARY KEY,
    content TEXT NOT NULL,
    FOREIGN KEY (track_id) REFERENCES track (id)
);
