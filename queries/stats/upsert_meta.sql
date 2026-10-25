INSERT INTO listen_track_meta (track_key, title, artist, album)
VALUES ($1, $2, $3, $4)
ON CONFLICT (track_key) DO UPDATE
SET title = excluded.title,
    artist = excluded.artist,
    -- 空 album 是「这次播放没拿到专辑」（刷新重登记前注册表缺该曲、队列旁表
    -- 无 album 字段），不是「这首歌没有专辑」：沿用库内既有值，不让空串把
    -- 正确专辑抹掉——该曲会从按专辑统计消失。title 维持整行级守卫不动。
    album = CASE WHEN excluded.album != '' THEN excluded.album ELSE listen_track_meta.album END
WHERE excluded.title != '';
