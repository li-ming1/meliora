SELECT
    id,
    title_sortable
FROM
    album
ORDER BY
    label COLLATE NOCASE ASC,
    catalog_number COLLATE NOCASE ASC,
    release_date ASC;
