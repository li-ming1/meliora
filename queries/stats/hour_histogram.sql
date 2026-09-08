SELECT CAST(strftime('%H', ts, 'unixepoch', 'localtime') AS INTEGER) AS hour,
       SUM(seconds) AS total
FROM listen_event
GROUP BY hour;
