-- Top tracks, aggregated by song rather than by source.
--
-- One song can be reached through several `track_key` identities: a local file
-- (`local:<path>`), a KuGou stream (`kugou:<mix_song_id>`), or a bare URL hash
-- (`hash:<...>`). Grouping on that key
-- kept those apart, so listening to a song offline and then online counted as
-- two different songs - and because local keys embed the file path, moving or
-- renaming a file split its history too.
--
-- Rows are therefore grouped on the normalized title + artist, which folds
-- every copy of a song into one entry. Normalization is deliberately limited to
-- case and surrounding whitespace: stripping things like "(feat. X)" or
-- "- Remastered" would merge genuinely distinct recordings.
--
-- Tracks whose metadata never resolved keep one row per track_key instead -
-- folding them together would collapse unrelated files into a single blank
-- entry.
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
        -- 确定性终键，否则两次刷新可能换行/随机进出榜单
        title ASC, artist ASC
LIMIT 10;
