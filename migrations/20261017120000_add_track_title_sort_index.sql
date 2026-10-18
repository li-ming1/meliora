-- Serves find_tracks_title_asc.sql / find_tracks_title_desc.sql, i.e. the
-- library track table's title sort:
--     ORDER BY title_sortable, album_id, location COLLATE NOCASE
-- Without an index the planner can only take `SCAN t` plus a temp B-tree.
--
-- Measured on the sqlx path itself (library::db::tests::report_title_sort_index_benefit:
-- two byte-identical 60k-track databases, only one carrying this index, A/B order
-- swapped every rep, min of 4, B faster in every single rep):
--   asc  91.1 ms -> 73.7 ms  = 1.24x
--   desc 90.8 ms -> 78.6 ms  = 1.16x
-- Both directions read `SCAN t USING COVERING INDEX idx_track_title_sort` with no
-- temp B-tree. One index covers both because the desc variant sorts *all three*
-- keys DESC (fixed 2026-10-17), which SQLite serves by scanning the same index
-- backwards; a "first key DESC, ties ASC" mix would have needed a second index.
--
-- The collation is written per key on purpose: an index over plain `location`
-- does NOT satisfy this ORDER BY (measured: still `USE TEMP B-TREE FOR RIGHT
-- PART OF ORDER BY`).
CREATE INDEX IF NOT EXISTS idx_track_title_sort
ON track (title_sortable, album_id, location COLLATE NOCASE);
