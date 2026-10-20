-- 同名重命名是空转：歌单表没有时间戳类列，值相等就不重写（用户在输入框里
-- 直接确认原名是常见操作）。
UPDATE playlist SET name = $1 WHERE id = $2 AND name != $1;