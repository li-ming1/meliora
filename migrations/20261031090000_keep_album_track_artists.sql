-- 删除 update_track_album_adopt 触发器并回填被它清掉的曲目级歌手链接。
--
-- 该触发器（20260813000000_create_track_artist.sql 创建）在 track 从无专辑被
-- 收编进专辑时（UPDATE OF album_id 且 NULL→非 NULL）删除该曲目的全部
-- track_artist 行。上游 hummingbird 2026-08-18（同名迁移）判定为 bug 并删除
-- + 回填：曲目级署名与专辑级署名是并列的信贷来源（统计署名
-- find_track_artist_credits 同时读两处），收编动作不该清空曲目级署名。
-- Meliora 分叉时带着旧触发器，扫描侧还配了 get_track_artist_row 的
-- album_id IS NULL 过滤来绕开——查询侧随本次代码改动一并去除。
--
-- 回填分两类：artists JSON 数组逐元素建链；JSON 缺失/为空时回退 artist_names
-- 显示名建链。INSERT OR IGNORE 保证幂等（重复执行安全）。

DROP TRIGGER IF EXISTS update_track_album_adopt;

INSERT OR IGNORE INTO artist (name, name_sortable)
SELECT DISTINCT artist_name.value, artist_name.value
FROM track t, json_each(t.artists) AS artist_name
WHERE t.album_id IS NOT NULL
  AND TRIM(artist_name.value) != '';

INSERT OR IGNORE INTO track_artist (track_id, artist_id)
SELECT t.id, a.id
FROM track t, json_each(t.artists) AS artist_name
JOIN artist a ON a.name = artist_name.value
WHERE t.album_id IS NOT NULL
  AND TRIM(artist_name.value) != '';

INSERT OR IGNORE INTO artist (name, name_sortable)
SELECT DISTINCT t.artist_names, t.artist_names
FROM track t
WHERE t.album_id IS NOT NULL
  AND (t.artists IS NULL OR json_array_length(t.artists) = 0)
  AND t.artist_names IS NOT NULL
  AND TRIM(t.artist_names) != '';

INSERT OR IGNORE INTO track_artist (track_id, artist_id)
SELECT t.id, a.id
FROM track t
JOIN artist a ON a.name = t.artist_names
WHERE t.album_id IS NOT NULL
  AND (t.artists IS NULL OR json_array_length(t.artists) = 0)
  AND t.artist_names IS NOT NULL
  AND TRIM(t.artist_names) != '';
