-- 批量删除后把位置压缩回 1..N。内层 ORDER BY 由 (playlist_id, position) 索引服务；
-- 单趟 UPDATE 取代逐次删除的"其后每行 position = position - 1"重写。
-- 值守卫：删除点之前的行位置不变，相等就跳过，不再整张歌单白写一遍
-- （sqlite3 实测：6 行删中间 2 行，写 2 行 / 原 4 行，终态逐行相同）。
UPDATE playlist_item AS pi
SET position = r.new_position
FROM (
    SELECT id, ROW_NUMBER() OVER (ORDER BY position) AS new_position
    FROM playlist_item
    WHERE playlist_id = $1
) AS r
WHERE r.id = pi.id AND pi.position != r.new_position;