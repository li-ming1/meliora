-- Serves find_tracks_in_album.sql (WHERE album_id = $1 ORDER BY disc_number,
-- track_number): the existing (album_id, id) index cannot satisfy that
-- ordering, so every album open paid a temp B-tree sort per query.
CREATE INDEX IF NOT EXISTS idx_track_album_disc_track
ON track (album_id, disc_number, track_number);
