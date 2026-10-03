SELECT
    t.id,
    t.album_id,
    t.location
FROM
    track t
ORDER BY
    t.disc_number DESC,
    t.track_number DESC;
