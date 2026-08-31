SELECT
    id,
    title_sortable,
    release_date,
    date_precision
FROM
    album
ORDER BY
    release_date ASC,
    title_sortable COLLATE NOCASE ASC;
