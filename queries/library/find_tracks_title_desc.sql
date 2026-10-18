SELECT
    t.id,
    t.album_id,
    t.location
FROM
    track t
ORDER BY
    t.title_sortable DESC,
    t.album_id DESC,
    t.location COLLATE NOCASE DESC;
