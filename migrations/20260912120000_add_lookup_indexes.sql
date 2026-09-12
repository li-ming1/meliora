-- Serves playlist_item lookups by track_id: the FK cascade when a track row is
-- deleted (playlist_item.track_id -> track.id), repoint_playlist_items
-- (queries/scan/repoint_playlist_items.sql: WHERE track_id = $2) and
-- list_playlist_ids_for_track (queries/scan/list_playlist_ids_for_track.sql:
-- WHERE track_id IN (...)); all were full scans (the unique (playlist_id,
-- track_id) index cannot serve track_id-only predicates).
CREATE INDEX IF NOT EXISTS idx_playlist_item_track ON playlist_item (track_id);

-- Serves album_path lookups by path: relocate_album_folder
-- (queries/scan/relocate_album_folder.sql: WHERE path = $2) and the
-- delete_album_path_trigger body (migrations/20260712000000_fix_album_path_trigger.sql:
-- WHERE album_path.path = OLD.folder); the PK (album_id, disc_num) cannot
-- serve these.
CREATE INDEX IF NOT EXISTS idx_album_path_path ON album_path (path);

-- Serves track lookups by folder: the delete_album_path_trigger NOT EXISTS
-- subquery (track.folder = OLD.folder, migrations/20260712000000_fix_album_path_trigger.sql)
-- and list_tracks_in_folder_or_location (queries/scan/list_tracks_in_folder_or_location.sql:
-- WHERE folder = $1 ...).
CREATE INDEX IF NOT EXISTS idx_track_folder ON track (folder);

-- Serves find_artists_name_asc.sql (ORDER BY name_sortable COLLATE NOCASE ASC);
-- the index collation must match the query expression verbatim for SQLite to
-- satisfy the ordering from the index.
CREATE INDEX IF NOT EXISTS idx_artist_name_sortable ON artist (name_sortable COLLATE NOCASE);

-- Serves find_albums_title_asc.sql (ORDER BY title_sortable COLLATE NOCASE ASC);
-- same collation-matching requirement as idx_artist_name_sortable.
CREATE INDEX IF NOT EXISTS idx_album_title_sortable ON album (title_sortable COLLATE NOCASE);
