-- 热门曲目按歌曲聚合，而非按来源键聚合。
--
-- 同一首歌可能经多个 `track_key` 身份到达：本地文件（`local:<path>`）、
-- 酷狗流（`kugou:<mix_song_id>`）或裸 URL 哈希（`hash:<...>`）。若按该键
-- 分组，这三者互不相通——离线听一次再在线听同一首会被算成两首歌；且
-- 本地键内嵌文件路径，移动或重命名文件也会把它的收听历史劈成两份。
--
-- 因此行按规范化后的「标题 + 歌手」分组，把同一首歌的所有副本折叠成
-- 一条。规范化刻意只做大小写与首尾空白：剥离 "(feat. X)"、"- Remastered"
-- 之类后缀会把真正不同的录音合并掉。
--
-- 元数据从未解析成功的曲目仍按 track_key 各占一行——把它们折叠起来
-- 会把互不相关的文件挤成同一个空白条目。
SELECT MAX(COALESCE(m.title, '')) AS title,
       MAX(COALESCE(m.artist, '')) AS artist,
       SUM(e.seconds) AS total
FROM listen_event e
LEFT JOIN listen_track_meta m ON m.track_key = e.track_key
WHERE e.ts >= $1
GROUP BY CASE
             WHEN COALESCE(m.title, '') = '' THEN e.track_key
             ELSE lower(trim(m.title)) || ' | ' || lower(trim(m.artist))
         END
ORDER BY total DESC,
        -- 聚合秒数大量并列（60s 整行为主），并列组内与 LIMIT 截断成员需要
        -- 确定性终键，否则两次刷新可能换行/随机进出榜单。末级 track_key 补上
        -- 「未解析行」组的并列：这些组的 title/artist 恒为空串，只靠它们仍
        -- 会跨刷新换位（第五轮 5A-3）
        title ASC, artist ASC,
        MAX(e.track_key) ASC
LIMIT 10;
