-- 已经为空的行不重写：无封面专辑的每首曲目否则都会在每轮扫描里被重新赋 NULL。
UPDATE track SET artwork_id = NULL
WHERE album_id = $1 AND artwork_id IS NOT NULL;
