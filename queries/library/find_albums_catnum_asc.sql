SELECT
    id,
    title_sortable
FROM
    album
ORDER BY
    catalog_number COLLATE NOCASE ASC,
    release_date ASC;
