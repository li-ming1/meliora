-- Every (track_id, artist_id) credit pair: album-level credits (the track's
-- album's album_artists) plus track-level credits (track_artist). Mirrors
-- find_all_tracks_by_artist's credit semantics (both credit paths must be
-- included) but yields integer pairs against the track table's own ids, so
-- the caller joins them with the base (location, id, album_id) rows in Rust
-- instead of re-transferring each credit's location string over SQL.
SELECT
    t.id AS track_id,
    aa.artist_id
FROM
    track t
    JOIN album al ON t.album_id = al.id
    JOIN album_artist aa ON aa.album_id = al.id
UNION ALL
SELECT
    ta.track_id,
    ta.artist_id
FROM
    track_artist ta;
