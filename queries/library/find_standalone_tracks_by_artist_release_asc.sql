SELECT t.id, t.title, t.album_id, t.track_number, t.disc_number, t.duration,
       t.location, t.artist_names, t.disc_subtitle,
       0 AS track_source, t.release_date AS track_date, t.id AS track_album
FROM track t
JOIN track_artist ta ON ta.track_id = t.id
WHERE ta.artist_id = $1 AND t.album_id IS NULL
ORDER BY track_date ASC, track_album ASC, disc_number ASC, track_number ASC;
