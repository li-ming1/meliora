//! 元数据标签写入库与音乐库歌手匹配共用的文本归一化。

/// 归一化人名以做身份比对：转小写、按非字母数字切分、token 排序、
/// 用单个空格拼接。扫描器的歌手匹配与标签写入的歌手分组都以它为键，
/// 于是两侧对"同一个歌手"的判定保持一致。
pub fn token_key(value: &str) -> String {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.sort_unstable();
    tokens.join(" ")
}
