SELECT id, title, album_id, track_number, disc_number, duration,
       location, artist_names, disc_subtitle
FROM track
WHERE album_id = $1
ORDER BY disc_number ASC, track_number ASC;
