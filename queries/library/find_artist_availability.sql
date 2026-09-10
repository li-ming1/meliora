SELECT
    aa.artist_id,
    t.location
FROM
    track t
    JOIN album al ON t.album_id = al.id
    JOIN album_artist aa ON aa.album_id = al.id
UNION ALL
SELECT
    ta.artist_id,
    t.location
FROM
    track t
    JOIN track_artist ta ON ta.track_id = t.id;
