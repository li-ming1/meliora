//! 在线音源（kugou）会话凭据文件共用的读写；`provider` 只进日志
//! 文本，其措辞是既有的日志排查口径，不要随手改写。
//!
//! 除凭据文件外，client 逐字共享的小件也收编在此：`UserProfile`、
//! `gateway_http_builder`（网关超时）与 `credential_present`。各自协议
//! 逻辑（签名/cookie 合成）不在此层，也绝不合并。毒化锁恢复统一在
//! `crate::sync`。

use std::{path::Path, time::Duration};

use zed_reqwest::{Client, ClientBuilder};

pub fn load<T: serde::de::DeserializeOwned>(path: &Path, provider: &str) -> Option<T> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        // 凭据文件缺失是首次运行的正常路径，保持安静。
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
    // 先写临时文件再改名就位（与 `playback/session_storage.rs` 同一套路）：
    // 就地 truncate + 重写，进程在写的中途死掉会留下被截断的会话文件。
    // Windows 上 `fs::rename` 会替换已存在的目标，因此该交换在所有受支持
    // 平台都是原子的。
    let tmp = path.with_extension("json.tmp");
    // 会话文件里有 token 与签名流 URL；Unix 上按 umask 默认会是 0644，
    // 同机其他用户可读。Windows 的用户目录 ACL 已足够，保持原样。
    #[cfg(unix)]
    let written = {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| file.write_all(json.as_bytes()))
    };
    #[cfg(not(unix))]
    let written = std::fs::write(&tmp, json);
    if let Err(err) = written {
        tracing::warn!(%err, "failed to write {provider} session temp file");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        tracing::warn!(%err, "failed to persist {provider} session");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 已登录用户的资料缓存（昵称 + 头像）。provider 把它持久化在自己的会话文件
/// （`KugouSession`）里，首次拉取之后就不再需要网络调用；client 在其 `client`
/// 模块下再导出本类型，故既有的 `client::UserProfile` 引用路径继续可解析。
#[derive(Clone, Debug)]
pub struct UserProfile {
    pub nickname: String,
    pub avatar_url: String,
}

/// 在线网关共享 HTTP client 的 builder：每个请求都有上界（连接 10s / 总计
/// 30s），于是一个卡住的网关请求不会拖住待完成的抓取或登录轮询。返回
/// builder 而非成品 `Client`，因为 client 保留自己的 `.build()` 失败策略
/// （kugou 是 panic）。
pub fn gateway_http_builder() -> ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
}

/// provider 的登录凭据存在且非空时为 true；空字符串凭据算作未登录
/// （kugou 的 `token` 共用这一判定）。
pub fn credential_present(credential: Option<&str>) -> bool {
    credential.is_some_and(|token| !token.is_empty())
}

/// F022 基准：`save` 单次调用（serialize pretty + 写临时文件 + rename）的真实
/// 延迟。kugou 的会话变更方法都在持有 session `std::sync::Mutex`
/// 的临界区内执行 `save`（kugou/client.rs 的 store_login / logout /
/// update_user_profile / mark_claim_attempted / store_vip_detail 共五处），
/// 所以这个数字就是并发请求构造者在锁上空等的真实时长。
///
/// 负载构造：小负载 = generate() 设备身份 + 登录态字段（token/nickname 等，
/// 字符串长度对齐真实凭据）；大负载 = kugou 现实上界 —— `vip_detail` 携带
/// 40 条 `busi_vip` 产品数组（`store_vip_detail` 原样持久化、
/// `has_active_vip` 与设置页按 `/data/busi_vip` 扫描的那棵树）+ ~1.5KB 的
/// `t1` 刷新票据。
///
/// 注：播放会话（队列/播放进度）不经本模块 —— playback/session_storage.rs
/// 是另一套 async tmp+rename 实现，`save` 的全部调用方只有在线音源
/// client（已核对），故"1000 条 QueueItemData 的 playback_session"在本模块
/// 没有测量对象，不设项。
///
/// 排除出常规测试运行 —— 计时依赖环境，只作为人工读数报告有意义：
///
/// ```text
/// cargo test --release --features kugou bench_session_io -- --ignored --nocapture
/// ```
#[cfg(all(test, feature = "kugou"))]
mod bench {
    use std::path::Path;
    use std::time::Instant;

    use serde_json::{Value, json};

    use crate::test_support::TestDir;

    use crate::kugou::client::KugouSession;

    const WARMUP: usize = 10;
    const ITERATIONS: usize = 100;

    /// 先跑 `WARMUP` 次不计时，再对 `save` 计时 `ITERATIONS` 次，打印
    /// min/p50/avg/max（毫秒）摘要与序列化后的载荷大小。计的是整个 `save`
    /// 调用——序列化与写盘同样发生在 session 互斥锁内，两者都属于 F022 要问的
    /// 那个数字。
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

    /// 酷狗现实上界：小负载 + 携带 40 条 `busi_vip` 产品数组的
    /// `vip_detail`（设置页与 `has_active_vip` 实际消费的结构，前两条
    /// 置 is_vip=1 走活跃分支）。
    #[cfg(feature = "kugou")]
    fn kugou_large() -> KugouSession {
        let mut session = kugou_small();
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
