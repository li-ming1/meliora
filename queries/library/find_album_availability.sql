SELECT
    t.album_id,
    t.location
FROM
    track t
WHERE
    t.album_id IS NOT NULL;