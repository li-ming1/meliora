SELECT
    t.id,
    t.title_sortable,
    t.album_id,
    t.location
FROM
    track t
ORDER BY
    t.duration ASC;
