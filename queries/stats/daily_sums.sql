SELECT date(ts, 'unixepoch', 'localtime') AS day,
       SUM(seconds)                      AS total
FROM listen_event
WHERE ts >= $1
GROUP BY day
ORDER BY day;
