SELECT COALESCE(m.artist, '') AS artist,
       SUM(e.seconds) AS total
FROM listen_event e
LEFT JOIN listen_track_meta m ON m.track_key = e.track_key
WHERE e.ts >= $1
  AND COALESCE(m.artist, '') != ''
GROUP BY m.artist
ORDER BY total DESC
LIMIT 10;
