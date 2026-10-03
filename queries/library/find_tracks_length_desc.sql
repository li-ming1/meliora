SELECT
    t.id,
    t.album_id,
    t.location
FROM
    track t
ORDER BY
    t.duration DESC;
