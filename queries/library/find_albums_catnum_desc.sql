SELECT
    id,
    title_sortable
FROM
    album
ORDER BY
    catalog_number COLLATE NOCASE DESC,
    release_date ASC;
