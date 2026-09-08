SELECT COALESCE(m.album, '') AS album,
       SUM(e.seconds) AS total
FROM listen_event e
LEFT JOIN listen_track_meta m ON m.track_key = e.track_key
WHERE e.ts >= $1
  AND COALESCE(m.album, '') != ''
GROUP BY m.album
ORDER BY total DESC
LIMIT 10;
