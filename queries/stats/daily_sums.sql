SELECT date(ts, 'unixepoch', 'localtime') AS day,
       SUM(seconds)                      AS total
FROM listen_event
GROUP BY day
ORDER BY day;
