-- artwork_source 随行返回，让 examine_folder_art 能判定无封面的目录是否仍有可学之处：
-- 现任封面来源为内嵌(0) 且文件夹里没有封面文件的专辑，本轮扫描对其无可作为。
SELECT ap.album_id, ap.path, al.artwork_source
FROM album_path ap
JOIN album al ON al.id = ap.album_id
WHERE ap.disc_num IN (-1, 0, 1);
