-- 专辑按歌手排序（find_albums_artist_{asc,desc}.sql）的复合索引。
--
-- 上游 hummingbird 2026-08-21（迁移 20260821000000）同款，collation 换成本
-- 项目的 NOCASE（上游是 HB_SORT，本地化排序是刻意未跟的候选）。
--
-- 为什么不是 20261021090000 撤掉的那类反例：那两枚 track 索引输在
-- 「索引序 + 每行回表取长 location」比顺序 SCAN + 临时 B 树更差；本索引的
-- 排序三键 (artist_sort, release_date, id) 被完整满足（rowid 尾列恰为 id
-- 主键），纯索引扫描无 temp B-tree，回表列只有短字符串 title_sortable。
-- 实测（合成 6000 专辑、997 个歌手键、min-of-15）：
--   无索引 4.82ms（SCAN + TEMP B-TREE）→ 有索引 3.15ms（SCAN USING INDEX，
--   1.53×）；真实库 54 专辑两侧均 ~0.04ms 无感。写侧只加在最小的主表上。
--   收益随库规模线性放大（专辑数 ≈ 曲目数 / 10）。
CREATE INDEX IF NOT EXISTS idx_album_artist_sort
    ON album (artist_sort COLLATE NOCASE, release_date);
