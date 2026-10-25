-- 无专辑曲目专用的曲目封面清链接：finalize 的 clear_track_art.sql 按 album_id 匹配，
-- 而无专辑曲目不在按专辑收尾的范围内（touched 只收专辑 id），封面来源消失时由
-- update_metadata（database.rs）按曲目 id 清旧链接。值守卫让已经为空的行不重写——
-- 没有封面文件夹的每轮扫描都会路过这里。
UPDATE track SET artwork_id = NULL
WHERE id = $1 AND artwork_id IS NOT NULL;
