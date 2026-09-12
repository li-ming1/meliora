UPDATE playlist_item SET position = position - 1 WHERE playlist_id = $1 AND position > $2;
DELETE FROM playlist_item WHERE id = $3
