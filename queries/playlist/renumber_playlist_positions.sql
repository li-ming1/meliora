-- Compacts positions back to 1..N after a batch removal. The (playlist_id,
-- position) index serves the inner ORDER BY; a single pass replaces the
-- per-removal `position = position - 1` rewrites of every later row.
UPDATE playlist_item AS pi
SET position = r.new_position
FROM (
    SELECT id, ROW_NUMBER() OVER (ORDER BY position) AS new_position
    FROM playlist_item
    WHERE playlist_id = $1
) AS r
WHERE r.id = pi.id;
