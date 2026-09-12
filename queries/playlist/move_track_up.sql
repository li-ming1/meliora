UPDATE playlist_item SET position = position + 1 WHERE playlist_id = $4 AND position >= $1 AND position < $2;
UPDATE playlist_item SET position = $1 WHERE id = $3;
