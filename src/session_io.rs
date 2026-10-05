//! 在线音源（kugou/netease）会话凭据文件共用的读写；`provider` 只进日志
//! 文本，其措辞是既有的日志排查口径，不要随手改写。

use std::path::Path;

pub fn load<T: serde::de::DeserializeOwned>(path: &Path, provider: &str) -> Option<T> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        // Missing credentials is the normal first-run path; stay quiet.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(%err, "no {provider} session on disk yet");
            return None;
        }
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "failed to read {provider} session");
            return None;
        }
    };
    serde_json::from_str(&contents)
        .map_err(|err| tracing::warn!(%err, "failed to decode {provider} session"))
        .ok()
}

pub fn save<T: serde::Serialize + ?Sized>(value: &T, path: &Path, provider: &str) {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(%err, "failed to create {provider} session dir");
        return;
    }
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(%err, "failed to serialize {provider} session");
            return;
        }
    };
    // Write to a temporary file and rename it into place (same pattern as
    // `playback/session_storage.rs`): an in-place truncate+rewrite can
    // leave a truncated session file behind when the process dies mid-write.
    // `fs::rename` replaces an existing target on Windows, so the swap is
    // atomic on every supported platform.
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp, json) {
        tracing::warn!(%err, "failed to write {provider} session temp file");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        tracing::warn!(%err, "failed to persist {provider} session");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// F022 基准：`save` 单次调用（serialize pretty + 写临时文件 + rename）的真实
/// 延迟。kugou/netease 的会话变更方法都在持有 session `std::sync::Mutex`
/// 的临界区内执行 `save`（kugou/client.rs 的 store_login / logout /
/// update_user_profile / mark_claim_attempted / store_vip_detail 共五处，
/// netease/client.rs 的 `save_session` 一处），所以这个数字就是并发请求
/// 构造者在锁上空等的真实时长。
///
/// 负载构造：小负载 = generate() 设备身份 + 登录态字段（token/nickname 等，
/// 字符串长度对齐真实凭据）；大负载 = kugou 现实上界 —— `vip_detail` 携带
/// 40 条 `busi_vip` 产品数组（`store_vip_detail` 原样持久化、
/// `has_active_vip` 与设置页按 `/data/busi_vip` 扫描的那棵树）+ ~1.5KB 的
/// `t1` 刷新票据。NeteaseSession 没有集合字段，序列化体积天然有界，"大
/// 负载"一项仅对 kugou 存在。
///
/// 注：播放会话（队列/播放进度）不经本模块 —— playback/session_storage.rs
/// 是另一套 async tmp+rename 实现，`save` 的全部调用方只有两个在线音源
/// client（已核对），故"1000 条 QueueItemData 的 playback_session"在本模块
/// 没有测量对象，不设项。
///
/// 排除出常规测试运行 —— 计时依赖环境，只作为人工读数报告有意义：
///
/// ```text
/// cargo test --release --features kugou bench_session_io -- --ignored --nocapture
/// ```
#[cfg(all(test, any(feature = "kugou", feature = "netease")))]
mod bench {
    use std::path::Path;
    use std::time::Instant;

    #[cfg(feature = "kugou")]
    use serde_json::{Value, json};

    use crate::test_support::TestDir;

    #[cfg(feature = "kugou")]
    use crate::kugou::client::KugouSession;
    #[cfg(feature = "netease")]
    use crate::netease::client::NeteaseSession;

    const WARMUP: usize = 10;
    const ITERATIONS: usize = 100;

    /// Times `save` `ITERATIONS` times after `WARMUP` untimed runs and prints
    /// a min/p50/avg/max summary in milliseconds plus the serialized payload
    /// size. The whole `save` call is timed — serialization happens under the
    /// session mutex exactly like the disk writes, so both belong in the
    /// number F022 is about.
    fn summarize<T: serde::Serialize + ?Sized>(name: &str, path: &Path, provider: &str, value: &T) {
        let bytes = serde_json::to_string_pretty(value)
            .map(|json| json.len())
            .unwrap_or_default();
        for _ in 0..WARMUP {
            super::save(value, path, provider);
        }
        let mut samples = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let start = Instant::now();
            super::save(value, path, provider);
            samples.push(start.elapsed().as_secs_f64() * 1_000.0);
        }
        samples.sort_by(|a, b| a.total_cmp(b));
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        println!(
            "{name:<34} payload={bytes:>6}B  min={:>7.3}ms  p50={:>7.3}ms  avg={:>7.3}ms  max={:>7.3}ms",
            samples[0],
            samples[samples.len() / 2],
            mean,
            samples[samples.len() - 1],
        );
    }

    /// 网易已登录会话：generate() 设备身份 + QR 登录回填的字段。各字符串
    /// 长度对齐真实凭据（52 位大写十六进制设备 id、64 位 hex nuid/token、
    /// ~41 位 NMTID、~55 字符头像 URL）。
    #[cfg(feature = "netease")]
    fn netease_logged_in() -> NeteaseSession {
        let mut session = NeteaseSession::generate();
        session.music_a = Some("a".repeat(64));
        session.music_u = Some("u".repeat(64));
        session.csrf = Some("c".repeat(64));
        session.user_id = Some(32953014);
        session.nickname = Some("基准用户".to_string());
        session.avatar_url =
            Some("http://p1.music.126.net/AAAAAAAAAAAAAAAAAAAAAAAA/000000000000.jpg".to_string());
        session.nmtid = Some(format!("00O{}", "n".repeat(38)));
        session
    }

    /// 酷狗登录态会话：generate() 设备身份（guid/mid/dev/dfid）+ ~256 字符
    /// 的不透明登录 token + 资料字段。
    #[cfg(feature = "kugou")]
    fn kugou_small() -> KugouSession {
        let mut session = KugouSession::generate();
        session.token = Some("t".repeat(256));
        session.userid = Some(123456789);
        session.nickname = Some("基准用户".to_string());
        session.avatar_url = Some("http://imge.kugou.com/stdmusic/000000000000.jpg".to_string());
        session
    }

    /// 酷狗现实上界：小负载 + ~1.5KB 的 `t1` 刷新票据 + 携带 40 条
    /// `busi_vip` 产品数组的 `vip_detail`（设置页与 `has_active_vip` 实际
    /// 消费的结构，前两条置 is_vip=1 走活跃分支）。
    #[cfg(feature = "kugou")]
    fn kugou_large() -> KugouSession {
        let mut session = kugou_small();
        session.t1 = Some("1".repeat(1536));
        session.vip_type = Some(1);
        session.vip_token = Some("v".repeat(64));
        session.last_claim_day = Some("2026-10-05".to_string());
        let products: Vec<Value> = (0..40)
            .map(|i| {
                json!({
                    "is_vip": if i < 2 { 1 } else { 0 },
                    "product_type": if i % 2 == 0 { "svip" } else { "vip" },
                    "product_name": format!("Meliora 会员产品 {i:02}"),
                    "vip_end_time": "2027-12-31 23:59:59",
                    "vip_level": i as i64,
                    "exp": 1_234_567 + i as i64,
                })
            })
            .collect();
        session.vip_detail = Some(json!({
            "status": 1,
            "error_code": 0,
            "data": {
                "is_vip": 0,
                "vip_end_time": "2027-12-31 23:59:59",
                "busi_vip": products,
            },
        }));
        session
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn bench_session_io_save_costs() {
        let dir = TestDir::new("session-io-bench");
        println!(
            "F022: session_io::save 单次耗时（serialize pretty + tmp 写 + rename），\
             预热 {WARMUP} 次 + 计时 {ITERATIONS} 次，普通测试线程："
        );
        #[cfg(feature = "netease")]
        summarize(
            "netease 小负载（已登录会话）",
            &dir.join("netease_session.json"),
            "netease",
            &netease_logged_in(),
        );
        #[cfg(feature = "kugou")]
        {
            summarize(
                "kugou 小负载（登录态会话）",
                &dir.join("kugou_session.json"),
                "kugou",
                &kugou_small(),
            );
            summarize(
                "kugou 大负载（vip_detail x40 + t1）",
                &dir.join("kugou_session_large.json"),
                "kugou",
                &kugou_large(),
            );
        }
    }
}
