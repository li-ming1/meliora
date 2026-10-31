INSERT INTO listen_track_meta (track_key, title, artist, album)
VALUES ($1, $2, $3, $4)
ON CONFLICT (track_key) DO UPDATE
SET title = excluded.title,
    -- artist 与 album 同理：在线端点的艺人字段形态不一（ui/kugou.rs
    -- artist_field 列了 8 个候选拼写），空串是「这次没拿到」，不是「无艺人」
    -- ——沿用库内既有值，不让空串把好 artist 抹出按艺人统计。
    artist = CASE WHEN excluded.artist != '' THEN excluded.artist ELSE listen_track_meta.artist END,
    -- 空 album 是「这次播放没拿到专辑」（刷新重登记前注册表缺该曲、队列旁表
    -- 无 album 字段），不是「这首歌没有专辑」：沿用库内既有值，不让空串把
    -- 正确专辑抹掉——该曲会从按专辑统计消失。title 维持整行级守卫不动。
    album = CASE WHEN excluded.album != '' THEN excluded.album ELSE listen_track_meta.album END
WHERE excluded.title != '';
