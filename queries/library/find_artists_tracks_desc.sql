SELECT a.id FROM artist a
LEFT JOIN (SELECT aa.artist_id AS id, COUNT(DISTINCT t.id) AS n
           FROM album_artist aa
           JOIN track t ON t.album_id = aa.album_id
           GROUP BY aa.artist_id) at ON at.id = a.id
LEFT JOIN (SELECT ta.artist_id AS id, COUNT(DISTINCT ta.track_id) AS n
           FROM track_artist ta
           GROUP BY ta.artist_id) tc ON tc.id = a.id
GROUP BY a.id
ORDER BY COALESCE(at.n, 0) + COALESCE(tc.n, 0) DESC,
         a.name_sortable COLLATE NOCASE ASC;
