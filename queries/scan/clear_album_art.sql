-- 已经为空的行不重写：无封面的专辑每轮扫描都会走到这里，NULL→NULL 不改变任何
-- 值，却会占一页 WAL 并唤醒读者。
UPDATE album SET artwork_id = NULL, artwork_source = 0
WHERE id = $1 AND (artwork_id IS NOT NULL OR artwork_source != 0);
