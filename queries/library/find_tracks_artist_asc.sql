-- Default track-table ordering (table.rs `None => ArtistAsc`).
--
-- Design tradeoff, measured and accepted (2026-10-15 audit): the COALESCE
-- across album/track artist_sort + artist_names cannot be served by any
-- single index, so every load of this sort pays a full-table JOIN + sort.
-- It runs on the runtime (`Table::reload_rows` spawn + generation guard)
-- with the `table rows loaded` elapsed_ms log for monitoring; the UI thread
-- never blocks on it. A generated column + composite index was evaluated
-- and rejected: the tie-break needs album.title_sortable, which an index
-- cannot fully cover, so the migration (create_track write path + backfill)
-- would cost more than the measured sort time at library sizes seen so far
-- (GPUI HARDCORE PERFORMANCE §38/§39).
SELECT
    t.id,
    t.album_id,
    t.location
FROM
    track t
    LEFT JOIN album al ON t.album_id = al.id
ORDER BY
    COALESCE(al.artist_sort, t.artist_sort, t.artist_names) COLLATE NOCASE ASC,
    al.title_sortable COLLATE NOCASE ASC,
    t.disc_number ASC,
    t.track_number ASC;
