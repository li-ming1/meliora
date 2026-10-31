UPDATE album
SET artist_display_override = COALESCE($2, '')
WHERE id = $1;
