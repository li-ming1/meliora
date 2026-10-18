-- artwork_source rides along so examine_folder_art can tell whether a directory
-- with no cover still has something to learn: an album whose incumbent is
-- embedded art (0) and whose folder holds no cover cannot change this scan.
SELECT ap.album_id, ap.path, al.artwork_source
FROM album_path ap
JOIN album al ON al.id = ap.album_id
WHERE ap.disc_num IN (-1, 0, 1);
