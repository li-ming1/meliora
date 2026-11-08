//! 沉浸页背景的纯 CPU 渲染管线（v4 清晰裁剪带：cover 居中裁剪 → 3×3
//! 中值去噪 → 重采样 → 阈值 unsharp）。自 managed_image.rs 原样搬出的
//! 自包含图像处理段：除以 `gpui::RenderImage` 作为像素容器外不触任何
//! gpui 行为，亦无共享状态。对外仅 `pub(super)`（宿主子树可见），
//! 无 crate 级放宽。

use std::sync::Arc;

use gpui::RenderImage;
use image::Frame;
use smallvec::smallvec;

/// 重采样后背景的最长边——对齐 1920 宽全屏。已达到或超过此尺寸的源原样直通：它们以 ~1:1
/// 或缩采样绘制，其细节本身就是可获得的最大锐度，任何滤波（包括为 JPEG 块准备的中值）只会
/// 融化真实的 1-2px 细节——这正是实测到的"真 2048 背景比圆盘标签更糊"的缘由，圆盘从不跑
/// enhance。低于此值的都先中值去噪、Lanczos 放大到该尺寸再锐化，于是 GPU 永远不放大纹理。
pub(super) const ENHANCE_TARGET_PX: u32 = 1920;
/// 放大之后应用的 unsharp-mask 强度（见 [`sharpen_rgba`]）。
const ENHANCE_SHARPEN_AMOUNT: f32 = 0.6;
/// 以输出像素计的 unsharp-mask 模糊 sigma：7.5× Lanczos 放大下边缘过渡跨越若干像素，
/// 所以光晕半径必须随倍数走，不能停在 1px。
const ENHANCE_SHARPEN_SIGMA: f32 = 1.5;
/// 通道差低于此值（0-255）不做 unsharp，因此 Lanczos 振铃与传感器噪声不会被推成光晕。
const ENHANCE_SHARPEN_THRESHOLD: f32 = 3.0;

/// 沉浸页背景目标尺寸渲染（2026-09-29 第四版·清晰裁剪带）：解码全尺寸
/// 原图 → cover 居中裁剪到目标纵横比 → 3×3 中值去噪（拆掉 JPEG 块边与
/// 椒盐噪声）→ Lanczos3 重采样到精确 `(w, h)` 设备像素 → 阈值 unsharp
/// 补回锐度。元素 `ObjectFit::Cover` 绘制时缩放系数恰为 1.0，GPU 双线性
/// 按纹素中心逐像素取样，零重采样。
///
/// 用户定案（2026-09-29 四轮反馈）：模糊方向整体否决——512+σ/20 糊成
/// 纯色斑，1024+σ/180+压暗仍是"这样糊糊的真的好吗"，最终要求**背景与
/// 圆盘小封面同等清晰**。圆盘清晰的机理是 4× 缩采样把压缩噪声平均掉；
/// 背景要追平同级观感靠两件事：
/// - **源尽量大**：酷狗阶梯加 4096 档（见 `cover_art`），存在时 4096 带
///   缩到 ~1920 设备像素约 0.47×，平均效应与圆盘同级；
/// - **去噪在前、锐化在后**：源只有 2048 时是 0.94× 近 1:1 搬运，JPEG
///   块伤会原样上屏（v2"质量差"的根因）——先中值去噪拆块边，再阈值
///   unsharp（缩小 0.5/σ1.0，放大 0.6/σ1.5，阈值 3）补锐度；顺序保证
///   锐化放大的是真边缘而非块伤。
///
/// 几何仍是 cover 居中裁剪：方形封面铺满宽屏必然裁掉上下各 ~23.5%，这
/// 是全出血背景的物理前提，清晰化不改变它。
///
/// 成本纪律（2026-09-29 "切换不丝滑" 复盘）：4096 源的全管线 ~1s+，其中
/// **中值去噪在 ≥2× 缩采样时是纯浪费**——CatmullRom 在 2× 以上每输出像
/// 素平均 ≥8 源像素，8px 的 JPEG 块被直接平均掉（4096→1920 约 0.47×，
/// 正是圆盘级观感的来源），去噪只在缩采样比 <2×（如 2048→1920 的
/// 0.94× 搬运，块伤会原样上屏）和放大路径执行。缩小用 CatmullRom（双三
/// 次，~Lanczos3 一半成本，缩采样画质等价，锐度由后面的 unsharp 统一
/// 补），放大保持 Lanczos3。本函数是数百 ms 级同步 CPU，**必须经
/// `spawn_blocking` 执行**（见 `retrieve` 的 Backdrop 分支），不得在异
/// 步运行时线程内联调用——否则音频流/封面下载与它抢 worker，切歌卡顿。
///
/// 所有权语义：按值吃下源 `Arc`，成功路径在裁剪带搬运完成后立即 `drop`
/// 源（~67MB 的 4096² 全尺寸缓冲不陪跑后半程）；失败路径原样把源还給
/// 调用方（`Err`），调用方按原 fallback 语义直接采用它。整条调用由
/// [`BACKDROP_PIPELINE_GATE`] 串行化——同刻至多一条管线在飞。
pub(super) fn backdrop_sharp_fill(
    image: Arc<RenderImage>,
    target_w: u32,
    target_h: u32,
) -> Result<Arc<RenderImage>, Arc<RenderImage>> {
    let Some(bytes) = image.as_bytes(0) else {
        return Err(image);
    };
    let size = image.size(0);
    let (sw, sh) = (u32::from(size.width), u32::from(size.height));
    if sw == 0 || sh == 0 || target_w == 0 || target_h == 0 {
        return Err(image);
    }
    // cover: 保留能铺满目标纵横比的最大居中区域。
    let crop_w = (f64::from(sw).min(f64::from(sh) * f64::from(target_w) / f64::from(target_h)))
        .round()
        .max(1.0) as u32;
    let crop_h = (f64::from(sh).min(f64::from(sw) * f64::from(target_h) / f64::from(target_w)))
        .round()
        .max(1.0) as u32;
    // 裁剪带逐行直接从源字节搬运（x0/y0 与下方 cover 居中裁剪同式），不再
    // 整幅 from_raw 复制——4096² 源那 67MB 全尺寸过路拷贝只有裁剪带用得上。
    let stride = sw as usize * 4;
    if bytes.len() < sh as usize * stride {
        return Err(image);
    }
    let x0 = ((sw - crop_w) / 2) as usize;
    let y0 = ((sh - crop_h) / 2) as usize;
    let mut band = image::RgbaImage::new(crop_w, crop_h);
    let row_bytes = crop_w as usize * 4;
    for (row, dst) in band.chunks_exact_mut(row_bytes).enumerate() {
        let sy = y0 + row;
        dst.copy_from_slice(&bytes[sy * stride + x0 * 4..][..row_bytes]);
    }
    // 早释放：全尺寸源字节到这里已经没有第二个读者，drop 归还 ~67MB——
    // 去噪/重采样/锐化占管线后半程的数百 ms，不为其多驻留一个许可周期。
    // Err 返回发生在搬运之前（原样交还所有权），Ok 之后不存在失败路径，
    // 所以后半程无需再持有源。
    drop(image);
    // 中值去噪只在实际需要时执行：缩采样比 <2×（块边会在输出中存活）或
    // 放大（块会被拉伸涂抹）。≥2× 时重采样的平均效应接管，跳过省一半时间。
    let band_long = crop_w.max(crop_h);
    let target_long = target_w.max(target_h);
    if band_long < 2 * target_long {
        denoise_rgba(&mut band);
    }
    let mut out = image::imageops::resize(
        &band,
        target_w,
        target_h,
        if band_long >= target_long {
            image::imageops::FilterType::CatmullRom
        } else {
            image::imageops::FilterType::Lanczos3
        },
    );
    // 阈值 unsharp 补回重采样损失的锐度：缩小轻量档（0.5/σ1.0），放大
    // 档随倍数走（0.6/σ1.5）。阈值 3 以下不动，避免把残余噪声推成 halo。
    let upscale = target_w.max(target_h) > sw.max(sh);
    if upscale {
        sharpen_rgba_with(&mut out, 0.6, ENHANCE_SHARPEN_SIGMA);
    } else {
        sharpen_rgba_with(&mut out, 0.5, 1.0);
    }
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(out)])))
}

/// 定长背景增强——窗口设备像素目标未知时使用的 [`ImageCacheMode::Backdrop(None)`] 兜底
/// （精确目标路径是 [`backdrop_sharp_fill`]）。返回新的 `RenderImage`：已达到或超过
/// [`ENHANCE_TARGET_PX`] 的源原样直通；更小的源依次做 3×3 中值去噪（化掉 JPEG 块阶——7.5×
/// 放大后每个块会涂抹到 60px）、Lanczos 重采样到显示尺寸、再一次阈值 unsharp 补回边缘锐度。
/// 只动 RGB 通道，alpha 不碰。
pub(super) fn enhance_render_image(image: &Arc<RenderImage>) -> Option<Arc<RenderImage>> {
    let bytes = image.as_bytes(0)?;
    let size = image.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    if width == 0 || height == 0 {
        return Some(Arc::clone(image));
    }
    if width.max(height) >= ENHANCE_TARGET_PX {
        return Some(Arc::clone(image));
    }
    let mut rgba = image::RgbaImage::from_raw(width, height, bytes.to_vec())?;
    denoise_rgba(&mut rgba);
    let long_side = rgba.width().max(rgba.height());
    let scale = f64::from(ENHANCE_TARGET_PX) / f64::from(long_side);
    let new_width = ((f64::from(rgba.width()) * scale) as u32).max(1);
    let new_height = ((f64::from(rgba.height()) * scale) as u32).max(1);
    rgba = image::imageops::resize(
        &rgba,
        new_width,
        new_height,
        image::imageops::FilterType::Lanczos3,
    );
    sharpen_rgba(&mut rgba);
    Some(Arc::new(RenderImage::new(smallvec![Frame::new(rgba)])))
}

/// 对 RGB 原地做阈值 unsharp-mask：差值超过阈值时 `out = src + amount·(src − blur(src))`。
/// 运行在中值（化掉 JPEG 块边）与 Lanczos 放大之后，所以只提升真实边缘——它替换掉的无序
/// unsharp 曾把块阶直接放大成"黑框框"网格。alpha 不碰。
pub(super) fn sharpen_rgba(image: &mut image::RgbaImage) {
    sharpen_rgba_with(image, ENHANCE_SHARPEN_AMOUNT, ENHANCE_SHARPEN_SIGMA);
}

/// [`sharpen_rgba`] 的参数化内核：`amount` 缩放 unsharp 响应，`sigma` 设定以输出像素计的
/// 光晕半径。
///
/// 模糊基保持 `imageops::blur`。曾把它和下面中值的 `sort_unstable` 一起换成
/// `fast_blur`（Kovesi 三重盒式，u8 域）+ `select_nth_unstable`：前者确实把
/// 管线过路瞬态从两块整幅 `Vec<f32>`（1920×1080 RGBA ≈33MB×2）压到两块 u8
/// （≈8MB×2，省 ~49MB），但**同环境同进程序列的实测是整条管线慢 25-79%**
/// （2048→1080p 588.8 vs 833.9ms、1280 档 320.0 vs 574.1ms、4096 档 753.7 vs
/// 942.4ms；旧实现 588.8ms 与 2026-10-15 基线 566-592ms 吻合，可作环境未漂移
/// 的旁证）。两处改动是合并测得、未单独归因，因此整体回退。
///
/// 教训（写给下一轮）：那 49MB 是"用 ms 换 MB"的显式取舍，不是免费午餐；
/// 而且**跨进程的 bench 序列不可直接比**——第一次跑只有 2048/1280 两个用例，
/// 加了 4096 之后同进程后半段的用例普遍变慢，我就是这样把一次回归读成了
/// −28~31% 的收益。对照必须同环境、同序列、同进程。
fn sharpen_rgba_with(image: &mut image::RgbaImage, amount: f32, sigma: f32) {
    let blurred = image::imageops::blur(image, sigma);
    for (src, blur) in image.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let s = src.0[c] as f32;
            let diff = s - blur.0[c] as f32;
            if diff.abs() > ENHANCE_SHARPEN_THRESHOLD {
                src.0[c] = (s + amount * diff).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 对 RGB 原地做 3×3 中值滤波：每个输出通道取其 3×3 邻域的中值（边缘钳位到最近像素）。
/// 中值会化掉 JPEG 的 8×8 块阶与平坦区噪声，同时保持真实边缘完整——它替换掉的 unsharp 段
/// 做的正相反（把那些块边直接放大成"黑框框"网格）。
///
/// 行缓冲滑窗实现，不做整图 clone：三条缓冲里永远是采样那一刻的原始字节
/// （next 行只从写指针之后的未写行装载，prev/cur 靠 swap 轮转），输出与
/// "先整图快照再采样"逐字节一致。x 的边缘 clamp 每像素算一次
/// （saturating_sub ≡ clamp(x−1)，min(x+1,last) ≡ clamp(x+1)），y 的每行
/// 算一次；采样集合、sort_unstable 取 [4]、遍历顺序与原实现相同，alpha
/// 字节不写即保持原值。
pub(super) fn denoise_rgba(image: &mut image::RgbaImage) {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return;
    }
    let w = width as usize;
    let h = height as usize;
    let stride = w * 4;
    let last_y = h - 1;
    let last_x = w - 1;
    let bytes: &mut [u8] = image;
    fn copy_row(dst: &mut [u8], src: &[u8], src_y: usize, stride: usize) {
        let start = src_y * stride;
        dst.copy_from_slice(&src[start..start + stride]);
    }
    let mut prev_row = vec![0u8; stride];
    let mut cur_row = vec![0u8; stride];
    let mut next_row = vec![0u8; stride];
    copy_row(&mut prev_row, bytes, 0, stride);
    copy_row(&mut cur_row, bytes, 0, stride);
    copy_row(&mut next_row, bytes, 1.clamp(0, last_y), stride);
    for y in 0..h {
        let out_row = &mut bytes[y * stride..(y + 1) * stride];
        for x in 0..w {
            let xm = x.saturating_sub(1) * 4;
            let xc = x * 4;
            let xp = (x + 1).min(last_x) * 4;
            for (c, out_cell) in out_row[xc..xc + 3].iter_mut().enumerate() {
                let mut window = [
                    prev_row[xm + c],
                    prev_row[xc + c],
                    prev_row[xp + c],
                    cur_row[xm + c],
                    cur_row[xc + c],
                    cur_row[xp + c],
                    next_row[xm + c],
                    next_row[xc + c],
                    next_row[xp + c],
                ];
                window.sort_unstable();
                // 保持全排序写法：换成 `select_nth_unstable(4)` 与上面的
                // `fast_blur` 是**同一轮**改的、也是同一轮合并测得变慢，
                // 没有单独归因过这一处；在没有独立收益证据的前提下，
                // 不动一个已被实测拖累的函数体（§39）。
                *out_cell = window[4];
            }
        }
        if y + 1 < h {
            std::mem::swap(&mut prev_row, &mut cur_row);
            std::mem::swap(&mut cur_row, &mut next_row);
            copy_row(&mut next_row, bytes, (y + 2).clamp(0, last_y), stride);
        }
    }
}

#[cfg(test)]
mod bench {
    //! 沉浸页背景管线的段级耗时探针：CPU-1（去噪核替换）的出数载体，同时
    //! 给出 backdrop_sharp_fill 各段占比与端到端交叉核对。
    //!
    //! 两种源口径各测一遍（对应 backdrop_sharp_fill :103-105 的去噪分支），
    //! 目标统一 1920×1080（"对齐 1920 宽全屏"的代表档）：
    //! - 4096² 源：band 4096×2304，band_long 4096 ≥ 2×1920 → **跳过中值**
    //!   （0.47× 缩采样，重采样平均效应接管 JPEG 块伤）；
    //! - 2048² 源：band 2048×1152，band_long 2048 < 2×1920 → **走中值去噪**
    //!   （1.07× 近 1:1 搬运，块伤会原样上屏）。
    //! 去噪分支与滤波器都按 backdrop_sharp_fill 的同一条件式在运行时取，不
    //! 写死——口径漂移会直接体现在输出行里。
    //!
    //! CPU-1 判读（阈值写死备查，裁决时可修订）：去噪段占端到端 <15% 直接
    //! 否决换核（换核 2× 提速也只省端到端 <8%）；15-30% 灰区，需换核预期
    //! 提速 ≥2×；≥30% 明确标的。去噪占比 × 换核提速倍数 = 端到端可省上限。
    //! 4096 口径去噪占比恒为 0（已跳过），它的读数价值在重采样段与"≥2× 跳
    //! 过去噪省一半"的实证。
    //!
    //! 交叉核对：裁剪带搬运（纯 memcpy）单列一段，四段之和应≈端到端 mean；
    //! 残差（RenderImage::new/Arc 等小开销）>10% 说明分段口径与端到端脱节，
    //! 先查口径再信占比。端到端产出即弃（含输出分配与释放），生产里产出进
    //! 缓存驻留——交叉核本是同口径对比，无碍。
    //!
    //! A/B 纪律（sharpen_rgba_with 文档的教训）：对照必须同环境、同序列、
    //! 同进程，跨进程 bench 序列不可直接比。合成数据为固定种子噪声——三段
    //! 都是全像素规则遍历，成本与像素值基本无关，噪声即代表性输入。
    //!
    //! 已从常规测试运行中排除——耗时依赖环境，只作为给人读的报告才有意义：
    //!
    //! ```text
    //! cargo test --release --features kugou bench_backdrop_segments -- --ignored --nocapture
    //! ```

    use std::hint::black_box;

    use super::*;

    /// 端到端目标：1920×1080 全屏设备像素（数值恰为 ENHANCE_TARGET_PX，但
    /// 语义是窗口目标尺寸的代表档，二者只是巧合同值）。
    const TARGET_W: u32 = 1920;
    const TARGET_H: u32 = 1080;

    /// 重量级计时点（去噪/重采样/锐化/端到端，单轮估 40-300ms）：2 轮预热
    /// + 6 轮计时，单点累计 240ms-2.4s，稳过 200ms 稳定性下限；9 个计时点合
    /// 计墙钟 ~8s。
    const HEAVY_WARMUP: usize = 2;
    const HEAVY_RUNS: usize = 6;
    /// 裁剪带搬运（纯 memcpy + band 分配，估 3-15ms/轮）的轮数：30 轮补足
    /// 累计时长；它只服务交叉核对，不参与占比裁决。
    const LIGHT_WARMUP: usize = 4;
    const LIGHT_RUNS: usize = 30;
    /// 交叉核对残差的可接受上限（占端到端的百分比）。
    const CROSSCHECK_RESIDUAL_MAX_PCT: f64 = 10.0;

    /// 固定种子（任意常数，只求跨运行逐字节一致；禁止运行时随机）。
    const SEED: u64 = 0x4D45_4C49_4F52_4100;

    /// 固定种子 LCG（Knuth MMIX 常数）填充的 RGBA 噪声图。
    fn synthetic_rgba(w: u32, h: u32, seed: u64) -> image::RgbaImage {
        let mut state = seed;
        let bytes: Vec<u8> = (0..w as usize * h as usize * 4)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        image::RgbaImage::from_raw(w, h, bytes).unwrap()
    }

    /// backdrop_sharp_fill 裁剪带搬运段（:82-93）的同式复刻：cover 居中裁剪
    /// 逐行 `copy_from_slice`，含 band 的分配（生产同款 `RgbaImage::new`）。
    fn crop_band(src: &image::RgbaImage, crop_w: u32, crop_h: u32) -> image::RgbaImage {
        let (sw, sh) = src.dimensions();
        let stride = sw as usize * 4;
        let x0 = ((sw - crop_w) / 2) as usize;
        let y0 = ((sh - crop_h) / 2) as usize;
        let bytes = src.as_raw();
        let mut band = image::RgbaImage::new(crop_w, crop_h);
        let row_bytes = crop_w as usize * 4;
        for (row, dst) in band.chunks_exact_mut(row_bytes).enumerate() {
            let sy = y0 + row;
            dst.copy_from_slice(&bytes[sy * stride + x0 * 4..][..row_bytes]);
        }
        band
    }

    /// 段级计时：`warmup` 轮免计时后计 `runs` 轮，打印与
    /// `test_support::bench_summarize_n` 同形的摘要并返回 mean（ms）。返回
    /// 值是"分段之和 vs 端到端"交叉核对所必需（bench_summarize_n 不回传样
    /// 本）；格式刻意对齐，两处输出可同表判读。
    fn bench_segment(name: &str, warmup: usize, runs: usize, mut f: impl FnMut()) -> f64 {
        for _ in 0..warmup {
            f();
        }
        let mut samples = Vec::with_capacity(runs);
        for _ in 0..runs {
            let start = std::time::Instant::now();
            f();
            samples.push(start.elapsed().as_secs_f64() * 1e3);
        }
        samples.sort_by(|a, b| a.total_cmp(b));
        let mean = samples.iter().sum::<f64>() / runs as f64;
        if runs >= 10 {
            println!(
                "{name:<40} min={:>8.1}ms  p50={:>8.1}ms  p90={:>8.1}ms  mean={:>8.1}ms  ({runs} runs)",
                samples[0],
                samples[runs / 2],
                samples[runs * 9 / 10],
                mean,
            );
        } else {
            println!(
                "{name:<40} min={:>8.1}ms  mean={:>8.1}ms  ({runs} runs)",
                samples[0], mean,
            );
        }
        mean
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn bench_backdrop_segments() {
        for (src_long, label) in [(4096u32, "4096"), (2048, "2048")] {
            // cover 居中裁剪（backdrop_sharp_fill :74-79 同式）得出的 band 尺寸
            let crop_w = (f64::from(src_long)
                .min(f64::from(src_long) * f64::from(TARGET_W) / f64::from(TARGET_H)))
            .round()
            .max(1.0) as u32;
            let crop_h = (f64::from(src_long)
                .min(f64::from(src_long) * f64::from(TARGET_H) / f64::from(TARGET_W)))
            .round()
            .max(1.0) as u32;
            let band_long = crop_w.max(crop_h);
            // :103 的同一去噪分支条件
            let denoise_active = band_long < 2 * TARGET_W.max(TARGET_H);
            // :110-114 的同一滤波器条件
            let filter = if band_long >= TARGET_W.max(TARGET_H) {
                image::imageops::FilterType::CatmullRom
            } else {
                image::imageops::FilterType::Lanczos3
            };

            let rgba = synthetic_rgba(src_long, src_long, SEED);
            println!(
                "== {label}² 源 → {TARGET_W}×{TARGET_H}，band {crop_w}×{crop_h}，去噪{}，滤波 {filter:?} ==",
                if denoise_active { "开" } else { "跳过" },
            );

            // 段：裁剪带搬运（生产里每次进管线都从零分配 + 搬运，故每轮重建）
            let crop_mean = bench_segment(
                &format!("{label} crop_copy band {crop_w}x{crop_h}"),
                LIGHT_WARMUP,
                LIGHT_RUNS,
                || {
                    let band = crop_band(&rgba, crop_w, crop_h);
                    black_box(band);
                },
            );

            // 段：3×3 中值去噪（denoise_rgba 原地；成本与像素值无关，同一缓
            // 冲重复跑不漂移计量）。先让缓冲指针经 black_box 不透明逃逸，段
            // 内写入不可被"结果未使用"整体折叠。
            let mut band = crop_band(&rgba, crop_w, crop_h);
            black_box(&band);
            let denoise_mean = if denoise_active {
                Some(bench_segment(
                    &format!("{label} denoise_rgba band {crop_w}x{crop_h}"),
                    HEAVY_WARMUP,
                    HEAVY_RUNS,
                    || denoise_rgba(&mut band),
                ))
            } else {
                println!("{label} denoise_rgba: skipped（band_long {band_long} ≥ 2×target_long）");
                None
            };

            // 段：重采样——按管线数据流吃的是去噪后的同一 band（成本与像素
            // 值无关）；每轮输出经 black_box 消费防折叠。
            let resize_mean = bench_segment(
                &format!("{label} resize -> {TARGET_W}x{TARGET_H} {filter:?}"),
                HEAVY_WARMUP,
                HEAVY_RUNS,
                || {
                    let out = image::imageops::resize(&band, TARGET_W, TARGET_H, filter);
                    black_box(out);
                },
            );

            // 段：阈值 unsharp 锐化（:118-123 的同一分支——两口径源长边均 ≥
            // 目标长边，取非放大档 0.5/σ1.0；放大档 0.6/σ1.5 属另一口径，本
            // 载体未覆盖）。原地跑，重复执行只会让值继续过冲，成本不变。
            let (amount, sigma) = if TARGET_W.max(TARGET_H) > src_long {
                (0.6f32, ENHANCE_SHARPEN_SIGMA)
            } else {
                (0.5f32, 1.0f32)
            };
            let mut sharpened = image::RgbaImage::new(TARGET_W, TARGET_H);
            black_box(&sharpened);
            let sharpen_mean = bench_segment(
                &format!("{label} sharpen {amount}/σ{sigma:.1} {TARGET_W}x{TARGET_H}"),
                HEAVY_WARMUP,
                HEAVY_RUNS,
                || sharpen_rgba_with(&mut sharpened, amount, sigma),
            );

            // 段：端到端（真实函数；Arc::clone 进、源消费与产出即弃都在计时
            // 段内，与生产"产出进缓存驻留"的差异见上面交叉核对的口径说明）
            let src = Arc::new(RenderImage::new(smallvec![Frame::new(rgba)]));
            let e2e_mean = bench_segment(
                &format!("{label} end_to_end backdrop_sharp_fill"),
                HEAVY_WARMUP,
                HEAVY_RUNS,
                || {
                    let _ = backdrop_sharp_fill(Arc::clone(&src), TARGET_W, TARGET_H);
                },
            );

            // 交叉核对 + CPU-1 占比读数
            let denoise_ms = denoise_mean.unwrap_or(0.0);
            let seg_sum = crop_mean + denoise_ms + resize_mean + sharpen_mean;
            let residual = e2e_mean - seg_sum;
            println!(
                "cross-check: 四段之和 {seg_sum:.1}ms vs 端到端 {e2e_mean:.1}ms，残差 {residual:.1}ms（{:.1}%）",
                residual / e2e_mean * 100.0,
            );
            if residual.abs() > e2e_mean * CROSSCHECK_RESIDUAL_MAX_PCT / 100.0 {
                println!(
                    "WARNING: 残差超 {CROSSCHECK_RESIDUAL_MAX_PCT:.0}%——分段口径与端到端已脱节，先查口径再信占比"
                );
            }
            if let Some(d) = denoise_mean {
                println!(
                    "CPU-1 读数：{label} 口径去噪段占端到端 {:.1}%",
                    d / e2e_mean * 100.0,
                );
            }
        }
    }
}
