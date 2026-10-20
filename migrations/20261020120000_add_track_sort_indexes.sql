-- Serves the library track table's two remaining global sorts:
--   find_tracks_length_asc.sql / _desc.sql   ORDER BY t.duration
--   find_tracks_number_asc.sql / _desc.sql   ORDER BY t.disc_number, t.track_number
-- `track` had no index on either key, so both sorts paid SCAN plus a temp
-- B-tree over the largest table in the library. One index per sort key pair
-- covers both directions (SQLite scans an index backwards for DESC, the same
-- trick as 20261017120000's title index). The album-scoped (disc, track)
-- ordering stays on idx_track_album_disc_track, which is prefixed by album_id
-- and cannot serve the global ordering.
--
-- Read-side A/B lives in library::db::tests::report_track_sort_index_benefit
-- (60k rows, byte-identical pair, min-of-4 with A/B order swapped per rep).
-- Write side: two more indexes on the largest table add index maintenance to
-- every track UPSERT — the same shape 20261017120000 measured there; scanning
-- is a background pipeline, so the trade is accepted for these two
-- user-triggerable sorts.
CREATE INDEX IF NOT EXISTS idx_track_duration
ON track (duration);

CREATE INDEX IF NOT EXISTS idx_track_disc_track_number
ON track (disc_number, track_number);
