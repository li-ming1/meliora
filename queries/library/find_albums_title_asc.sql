SELECT
    id,
    title_sortable
FROM
    album
ORDER BY
    title_sortable COLLATE NOCASE ASC;
