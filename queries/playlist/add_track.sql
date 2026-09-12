INSERT INTO playlist_item (playlist_id, track_id, position)
	VALUES(
	    $1,
		$2,
		COALESCE((SELECT MAX(position) FROM playlist_item WHERE playlist_id = $1) + 1, 1)
	)
