SELECT t.title AS title,
       COALESCE(t.artist_names, '') AS artist,
       COALESCE(a.title, '') AS album
FROM track t
LEFT JOIN album a ON a.id = t.album_id
WHERE t.location = $1
LIMIT 1;
