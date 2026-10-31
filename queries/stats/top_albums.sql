SELECT COALESCE(m.album, '') AS album,
       SUM(e.seconds) AS total
FROM listen_event e
LEFT JOIN listen_track_meta m ON m.track_key = e.track_key
WHERE e.ts >= $1
  AND COALESCE(m.album, '') != ''
GROUP BY m.album
ORDER BY total DESC,
        -- 聚合秒数大量并列（60s 整行为主），并列组内与 LIMIT 截断成员需要
        -- 确定性终键，否则两次刷新可能换行/随机进出榜单
        album ASC
LIMIT 10;
