INSERT INTO listen_track_meta (track_key, title, artist, album)
VALUES ($1, $2, $3, $4)
ON CONFLICT (track_key) DO UPDATE
SET title = excluded.title,
    artist = excluded.artist,
    album = excluded.album
WHERE excluded.title != '';
