SELECT id, title, album_id, track_number, disc_number, duration,
       location, artist_names, disc_subtitle
FROM track
WHERE id = $1;
