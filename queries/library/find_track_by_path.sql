SELECT id, title, album_id, track_number, disc_number, duration,
       location, artist_names, disc_subtitle
FROM track
WHERE location = $1
LIMIT 1;
