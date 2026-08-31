CREATE INDEX IF NOT EXISTS idx_artist_album_id ON album (artist_id, id);
CREATE INDEX IF NOT EXISTS idx_track_album_id ON track (album_id, id);
