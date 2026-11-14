-- $2 为 NULL（专辑内混入完全无 artist tag 的文件，display_override =
-- album_artist.or(artist) 为 None）时保留专辑现有 override——与调用点
-- albums.rs 的注释及 sort tag 侧「无 tag 不清空」语义对齐；此前
-- COALESCE($2, '') 会把已有 override 清成空串，专辑展示名随扫描顺序摆动。
UPDATE album
SET artist_display_override = COALESCE($2, artist_display_override)
WHERE id = $1;
