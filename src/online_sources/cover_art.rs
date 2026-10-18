//! 全屏展示用的封面 URL 辅助。
//!
//! 搜索/队列载荷带的是*缩略图*封面 URL（酷狗给的是 `stdmusic/256`）。把它们铺满沉浸页背景
//! 会放大 4-8 倍，看起来一团糊。音源主机在同一张大图的已知 URL 形状上提供更大幅的变体：
//! 酷狗的 `stdmusic` 路径吃一个尺寸段（2026-09-28 实测：480/1280/1920/2048/4096 都能出
//! 真像素——但仅当该封面的大图变体确实生成过）。
//! [`display_cover_candidates`] 排出尺寸梯队（最大变体在前，原始缩略图在最后），
//! [`fetch_display_cover_bytes_capped`] 沿梯队往下走，保留真正出得了像素的最大变体，只有
//! 每一档都落空时才回落到原始缩略图。从 404 的 2048 静默跳到 256px 缩略图，正是 2026-09-29
//! 「沉浸页背景糊」那条反馈的根因——梯队加上命中赢家日志，让背景实际拿到的分辨率可观测。

use crate::sync::RecoveredMutex;
use std::{
    collections::HashSet,
    sync::{LazyLock, Mutex},
};

use anyhow::Context as _;
use tracing::{info, warn};

/// 在原始 URL 之前从最大到最小尝试的酷狗 `stdmusic` 尺寸档位。主机按请求的档位即时缩放
/// （2026-09-28 实测：480/1280/1920/2048/4096 都出真像素）——但不是每张封面都在服务端生成过
/// 每个变体，所以梯队保留确实存在的最大变体。自 2026-09-29「背景必须与圆盘小封面同等清晰」
/// 的指示起 4096 领衔梯队：4096 时背景的裁剪带以约 0.47× 重采样到 ~1920 设备像素，其平均
/// 效应与 4× 缩采样的圆盘封面一致；缺失的变体只花一次 404 并按会话做负缓存。
const KUGOU_SIZE_LADDER: [&str; 4] = ["4096", "2048", "1280", "480"];

/// 本次会话里没返回过像素的展示变体 URL。一次 404 就是一次往返；没有这个集合的话，该封面
/// 每次背景取图都会在抵达缓存里的赢家之前重复付每一档失败的代价。刻意按会话作用域：下次
/// 启动会重试，所以 CDN 抽风能自愈。
static VARIANT_MISSES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 负缓存的条数上界。丢条目的代价只是"再付一次 404"，所以越界时整表清空——
/// 比逐条 LRU 简单，又不会让一次长会话把无界的 URL 集合留在内存里。
const VARIANT_MISSES_CAP: usize = 4096;

fn variant_missed(url: &str) -> bool {
    VARIANT_MISSES.lock_recovered().contains(url)
}

fn note_variant_miss(url: &str) {
    let mut misses = VARIANT_MISSES.lock_recovered();
    if misses.len() >= VARIANT_MISSES_CAP {
        misses.clear();
    }
    misses.insert(url.to_string());
}

/// `url` 的展示变体 URL，最大的在前，末尾是原始缩略图本身。保持纯函数以便梯队顺序可单测。
pub fn display_cover_candidates(url: &str) -> Vec<String> {
    let mut rungs = Vec::with_capacity(KUGOU_SIZE_LADDER.len() + 1);
    if url.contains("/stdmusic/") {
        for size in KUGOU_SIZE_LADDER {
            rungs.push(rewrite_kugou_stdmusic_with(url, size));
        }
    }
    rungs.push(url.to_string());
    rungs
}

/// 酷狗专辑封面位于 `…/stdmusic/{size}/{date}/{hash}.jpg`，尺寸档位有 150/240/256/400/480。
/// 把缩略图所带的档位抬到 `size`；`stdmusic/` 之后的部分一律不动。
fn rewrite_kugou_stdmusic_with(url: &str, size: &str) -> String {
    let marker = "/stdmusic/";
    let Some(pos) = url.find(marker).map(|pos| pos + marker.len()) else {
        return url.to_string();
    };
    let rest = &url[pos..];
    let Some(size_len) = rest.find('/') else {
        return url.to_string();
    };
    if !rest[..size_len].bytes().all(|b| b.is_ascii_digit()) || size_len == 0 {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len() + 2);
    out.push_str(&url[..pos]);
    out.push_str(size);
    out.push_str(&rest[size_len..]);
    out
}

/// 按从大到小走音源的尺寸梯队，返回真正出得了像素的最大变体，每一档都落空时回落到原始
/// 缩略图。失败的档位会按会话记住，所以之后同一封面的取图会直接跳到赢的尺寸。赢家与落空
/// 都走共享磁盘缓存，因此取原始缩略图的网格元素仍与本路径共享条目。
///
/// 梯队可以用 `max_side` 封顶（0 = 不封顶）：缩略图类消费方（512px 圆盘标签）不该为了小
/// 尺寸展示付一次完整 4096² 解码——显示×4 在源头已经过采样，而 2048 解码的成本只有 4096 的
/// 1/4（2026-09-29 审计）。尺寸解析不出来的档位（外来形状）一律放行。
pub async fn fetch_display_cover_bytes_capped(
    url: &str,
    max_side: u32,
) -> anyhow::Result<Option<bytes::Bytes>> {
    let mut last_error = None;
    for rung in display_cover_candidates(url) {
        if variant_missed(&rung) {
            continue;
        }
        if max_side > 0 && rung_size(&rung).is_some_and(|size| size > max_side) {
            continue;
        }
        match crate::media::http_source::http_cover_bytes_cached(&rung).await {
            Ok(Some(bytes)) => {
                if rung != url {
                    info!(
                        target: "backdrop",
                        bytes = bytes.len(),
                        url = %rung,
                        "display cover resolved to a large variant"
                    );
                }
                return Ok(Some(bytes));
            }
            Ok(None) => note_variant_miss(&rung),
            Err(error) => {
                // 瞬态失败（超时/DNS/TLS）不该把这一档整个会话拉黑：只有「服务端说没有像素」
                // （Ok(None)）才是真落空。
                warn!(target: "backdrop", %error, url = %rung, "display cover variant fetch failed");
                last_error = Some(error);
            }
        }
    }
    match last_error {
        Some(error) => Err(error).with_context(|| format!("fetching cover {url}")),
        None => Ok(None),
    }
}

/// 梯队某档 URL 的最大边长（当其形状带边长时）：酷狗 `…/stdmusic/{size}/…`。
/// 未知形状返回 None，永不被封顶掉。
fn rung_size(url: &str) -> Option<u32> {
    if let Some(rest) = url.split("/stdmusic/").nth(1) {
        return rest
            .split('/')
            .next()
            .and_then(|size| size.parse::<u32>().ok());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 梯队的最大档——旧的单次重写做的就是这件事。
    fn largest_variant(url: &str) -> String {
        display_cover_candidates(url).swap_remove(0)
    }

    #[test]
    fn kugou_stdmusic_bumps_to_display_size() {
        let url = "http://imge.kugou.com/stdmusic/256/20260306/20260306201650909877.jpg";
        assert_eq!(
            largest_variant(url),
            "http://imge.kugou.com/stdmusic/4096/20260306/20260306201650909877.jpg"
        );
        // 已经很大的 URL 仍然抬到顶档（2048 → 4096）。
        assert_eq!(
            largest_variant("https://imgessl.kugou.com/stdmusic/2048/20200101/abcdef.jpg"),
            "https://imgessl.kugou.com/stdmusic/4096/20200101/abcdef.jpg"
        );
    }

    #[test]
    fn kugou_ladder_walks_largest_to_original() {
        let url = "http://imge.kugou.com/stdmusic/256/20260306/x.jpg";
        let candidates = display_cover_candidates(url);
        assert_eq!(
            candidates,
            vec![
                "http://imge.kugou.com/stdmusic/4096/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/2048/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/1280/20260306/x.jpg".to_string(),
                "http://imge.kugou.com/stdmusic/480/20260306/x.jpg".to_string(),
                url.to_string(),
            ]
        );
    }

    #[test]
    fn unknown_shapes_pass_through() {
        let url = "https://example.com/cover/art.png";
        assert_eq!(largest_variant(url), url);
        assert_eq!(display_cover_candidates(url), vec![url.to_string()]);
        // 非数字的尺寸段保持原样。
        let weird = "http://imge.kugou.com/stdmusic/mid/20200101/x.jpg";
        assert_eq!(largest_variant(weird), weird);
    }

    #[test]
    fn rung_size_parses_ladder_shapes() {
        assert_eq!(
            rung_size("http://imge.kugou.com/stdmusic/4096/20260306/x.jpg"),
            Some(4096)
        );
        assert_eq!(
            rung_size("http://imge.kugou.com/stdmusic/256/20260306/x.jpg"),
            Some(256)
        );
        // 未知形状永不被封顶。
        assert_eq!(rung_size("https://example.com/cover/art.png"), None);
    }
}
