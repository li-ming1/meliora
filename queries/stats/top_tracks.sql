SELECT e.track_key AS track_key,
       COALESCE(m.title, '') AS title,
       COALESCE(m.artist, '') AS artist,
       SUM(e.seconds) AS total
FROM listen_event e
LEFT JOIN listen_track_meta m ON m.track_key = e.track_key
WHERE e.ts >= $1
GROUP BY e.track_key
ORDER BY total DESC
LIMIT 10;
