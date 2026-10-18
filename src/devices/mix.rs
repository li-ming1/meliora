use smallvec::SmallVec;
use tracing::{info, warn};

use crate::devices::channels::{ChannelLabel, ChannelLayout, ChannelPosition};

pub const SURROUND_ATTENUATION: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixOptions {
    pub lfe_to_stereo: bool,
    pub surround_attenuation: f64,
}

impl Default for MixOptions {
    fn default() -> Self {
        Self {
            lfe_to_stereo: false,
            surround_attenuation: SURROUND_ATTENUATION,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MixMatrix {
    /// 行为 `out_channels`、列为 `in_channels`
    rows: Vec<Vec<f64>>,
    /// 每输出行的非零 (输入索引, 权重)，按输入索引升序。
    /// 必须在 normalize 之后派生（缩放改变权重终值）；
    /// 过滤条件与 `w != 0.0` 完全一致，-0.0 一并剔除以保证逐位等价
    taps: Vec<Vec<(u16, f64)>>,
    in_channels: usize,
    out_channels: usize,
}

impl MixMatrix {
    fn in_channels(&self) -> usize {
        self.in_channels
    }

    fn out_channels(&self) -> usize {
        self.out_channels
    }

    /// 统一构造入口：构造即派生 taps，保证任何已存在的矩阵 taps 与 rows 一致
    fn from_rows(rows: Vec<Vec<f64>>, in_channels: usize, out_channels: usize) -> Self {
        let mut matrix = Self {
            rows,
            in_channels,
            out_channels,
            taps: Vec::new(),
        };
        matrix.derive_taps();
        matrix
    }

    /// 从 rows 派生稀疏 tap 表。过滤条件与原 `w != 0.0` 分支完全一致
    /// （IEEE 下 -0.0 != 0.0 为 false，同样剔除），必须在权重终值确定后调用
    fn derive_taps(&mut self) {
        self.taps = self
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .filter(|&(_, &w)| w != 0.0)
                    .map(|(j, &w)| {
                        debug_assert!(j <= u16::MAX as usize);
                        (j as u16, w)
                    })
                    .collect()
            })
            .collect();
    }

    #[cfg(test)]
    fn rows(&self) -> &[Vec<f64>] {
        &self.rows
    }

    fn is_identity(&self) -> bool {
        if self.in_channels != self.out_channels {
            return false;
        }
        for (i, row) in self.rows.iter().enumerate() {
            for (j, &w) in row.iter().enumerate() {
                let expected = if i == j { 1.0 } else { 0.0 };
                if (w - expected).abs() > f64::EPSILON {
                    return false;
                }
            }
        }
        true
    }

    /// 由源布局与目标布局构建混合矩阵。
    ///
    /// 任一侧缺少位置信息时，退化为通用的按声道数矩阵。
    fn build(src: &ChannelLayout, dst: &ChannelLayout, opts: MixOptions) -> MixMatrix {
        let mut matrix = match (src, dst) {
            (ChannelLayout::Positioned(src_pos), ChannelLayout::Positioned(dst_pos)) => {
                build_positioned(*src_pos, *dst_pos, opts)
            }
            _ => build_count_based(src.count(), dst.count(), opts),
        };
        matrix.normalize();
        // taps 必须在 normalize 之后（重）派生：缩放改变权重终值
        matrix.derive_taps();
        matrix
    }

    /// 当任一输出行的权重和超过 1.0 时整体缩小矩阵，使满幅输入永远不可能削顶。
    /// 缩放对所有行统一施加，以保持声道间的平衡。
    fn normalize(&mut self) {
        let max_row_sum = self
            .rows
            .iter()
            .map(|row| row.iter().map(|w| w.abs()).sum::<f64>())
            .fold(0.0, f64::max);
        if max_row_sum > 1.0 {
            for row in &mut self.rows {
                for w in row {
                    *w /= max_row_sum;
                }
            }
        }
    }

    /// 混合一帧平面样本。
    ///
    /// `input` 与 `output` 的长度必须分别是 `in_channels` 与 `out_channels`。
    /// `output` 被覆写（不是累加）。
    fn mix_frame(&self, input: &[f64], output: &mut [f64]) {
        debug_assert_eq!(input.len(), self.in_channels);
        debug_assert_eq!(output.len(), self.out_channels);

        // 只遍历非零 tap；taps 按输入索引升序，与原全矩阵扫描累加顺序一致，逐位等价
        for (i, row_taps) in self.taps.iter().enumerate() {
            let mut acc = 0.0f64;
            for &(j, w) in row_taps {
                acc += w * input[j as usize];
            }
            // normalize 已保证行和 <= 1.0，这里的钳位纯属安全网
            output[i] = acc.clamp(-1.0, 1.0);
        }
    }
}

/// 在位置式布局的声道顺序里查单个位置位所对应的规范下标
/// （按位升序，与 [`ChannelPosition::positions`] 一致）。
fn position_index(haystack: ChannelPosition, needle: ChannelPosition) -> Option<usize> {
    if needle.bits().count_ones() != 1 || !haystack.contains(needle) {
        return None;
    }
    // 严格低于 needle 那一位的置位个数即为下标。
    let below = haystack.bits() & (needle.bits() - 1);
    Some(below.count_ones() as usize)
}

fn build_positioned(src: ChannelPosition, dst: ChannelPosition, opts: MixOptions) -> MixMatrix {
    // 有些解码器把单声道报成 FRONT_LEFT；统一归一化，让它两个声道都出声。
    let src = if src.bits().is_power_of_two() {
        ChannelPosition::FRONT_CENTER
    } else {
        src
    };

    let in_channels = src.bits().count_ones() as usize;
    let out_channels = dst.bits().count_ones() as usize;

    let mut rows = vec![vec![0.0f64; in_channels]; out_channels];

    let a = opts.surround_attenuation;

    let mut route = |out_pos: ChannelPosition, in_pos: ChannelPosition, weight: f64| {
        if weight == 0.0 {
            return;
        }
        let (Some(oi), Some(ii)) = (position_index(dst, out_pos), position_index(src, in_pos))
        else {
            return;
        };
        rows[oi][ii] += weight;
    };

    // 目标没有对应槽位的源声道需要改道。
    let unmapped = |pos: ChannelPosition| src.contains(pos) && !dst.contains(pos);

    let has_l = dst.contains(ChannelPosition::FRONT_LEFT);
    let has_r = dst.contains(ChannelPosition::FRONT_RIGHT);
    let has_c = dst.contains(ChannelPosition::FRONT_CENTER);
    let has_rl = dst.contains(ChannelPosition::REAR_LEFT);
    let has_rr = dst.contains(ChannelPosition::REAR_RIGHT);

    let src_only_mono = src == ChannelPosition::FRONT_CENTER;

    for in_pos in src.positions() {
        if dst.contains(in_pos) {
            route(in_pos, in_pos, 1.0);
        }
    }

    // 非单声道的中置：目标没有中置时折进 L/R。
    if unmapped(ChannelPosition::FRONT_CENTER) && !src_only_mono {
        if has_l {
            route(
                ChannelPosition::FRONT_LEFT,
                ChannelPosition::FRONT_CENTER,
                a,
            );
        }
        if has_r {
            route(
                ChannelPosition::FRONT_RIGHT,
                ChannelPosition::FRONT_CENTER,
                a,
            );
        }
    }

    // 双声道转单声道：前置对取平均。
    if !has_l && !has_r && has_c {
        if src.contains(ChannelPosition::FRONT_LEFT) {
            route(
                ChannelPosition::FRONT_CENTER,
                ChannelPosition::FRONT_LEFT,
                0.5,
            );
        }
        if src.contains(ChannelPosition::FRONT_RIGHT) {
            route(
                ChannelPosition::FRONT_CENTER,
                ChannelPosition::FRONT_RIGHT,
                0.5,
            );
        }
    }

    if unmapped(ChannelPosition::SIDE_LEFT) {
        if has_rl {
            route(ChannelPosition::REAR_LEFT, ChannelPosition::SIDE_LEFT, a);
        } else if has_l {
            route(ChannelPosition::FRONT_LEFT, ChannelPosition::SIDE_LEFT, a);
        }
    }
    if unmapped(ChannelPosition::SIDE_RIGHT) {
        if has_rr {
            route(ChannelPosition::REAR_RIGHT, ChannelPosition::SIDE_RIGHT, a);
        } else if has_r {
            route(ChannelPosition::FRONT_RIGHT, ChannelPosition::SIDE_RIGHT, a);
        }
    }
    if unmapped(ChannelPosition::REAR_LEFT) && has_l {
        route(ChannelPosition::FRONT_LEFT, ChannelPosition::REAR_LEFT, a);
    }
    if unmapped(ChannelPosition::REAR_RIGHT) && has_r {
        route(ChannelPosition::FRONT_RIGHT, ChannelPosition::REAR_RIGHT, a);
    }
    if unmapped(ChannelPosition::REAR_CENTER) {
        if has_rl && has_rr {
            route(ChannelPosition::REAR_LEFT, ChannelPosition::REAR_CENTER, a);
            route(ChannelPosition::REAR_RIGHT, ChannelPosition::REAR_CENTER, a);
        } else if has_rl {
            route(ChannelPosition::REAR_LEFT, ChannelPosition::REAR_CENTER, a);
        } else if has_rr {
            route(ChannelPosition::REAR_RIGHT, ChannelPosition::REAR_CENTER, a);
        } else if has_l && has_r {
            route(ChannelPosition::FRONT_LEFT, ChannelPosition::REAR_CENTER, a);
            route(
                ChannelPosition::FRONT_RIGHT,
                ChannelPosition::REAR_CENTER,
                a,
            );
        } else if has_c {
            route(
                ChannelPosition::FRONT_CENTER,
                ChannelPosition::REAR_CENTER,
                a,
            );
        }
    }

    if opts.lfe_to_stereo && unmapped(ChannelPosition::LFE1) {
        if has_l {
            route(ChannelPosition::FRONT_LEFT, ChannelPosition::LFE1, a);
        }
        if has_r {
            route(ChannelPosition::FRONT_RIGHT, ChannelPosition::LFE1, a);
        }
    }

    // 单声道：没有中置输出时复制到 L/R。
    if src_only_mono && !has_c {
        if has_l {
            route(
                ChannelPosition::FRONT_LEFT,
                ChannelPosition::FRONT_CENTER,
                1.0,
            );
        }
        if has_r {
            route(
                ChannelPosition::FRONT_RIGHT,
                ChannelPosition::FRONT_CENTER,
                1.0,
            );
        }
    }

    MixMatrix::from_rows(rows, in_channels, out_channels)
}

/// 布局拿不到可用位置元数据时，退回只按声道数的通用矩阵。
fn build_count_based(in_count: usize, out_count: usize, _opts: MixOptions) -> MixMatrix {
    let mut rows = vec![vec![0.0f64; in_count]; out_count];

    if in_count == 0 || out_count == 0 {
        return MixMatrix::from_rows(rows, in_count, out_count);
    }

    if in_count == out_count {
        for (i, row) in rows.iter_mut().enumerate() {
            row[i] = 1.0;
        }
    } else if in_count == 1 && out_count == 2 {
        rows[0][0] = 1.0;
        rows[1][0] = 1.0;
    } else if in_count == 2 && out_count == 1 {
        rows[0][0] = 0.5;
        rows[0][1] = 0.5;
    } else if in_count < out_count {
        for (i, row) in rows.iter_mut().enumerate().take(in_count) {
            row[i] = 1.0;
        }
    } else {
        warn!(
            "No positional layout available for {} -> {} channel mix; using coarse count-based fallback",
            in_count, out_count
        );
        #[allow(clippy::needless_range_loop)]
        for j in 0..in_count {
            let target = j.min(out_count - 1);
            rows[target][j] += 1.0 / out_count.max(1) as f64;
        }
    }

    MixMatrix::from_rows(rows, in_count, out_count)
}

pub struct ChannelMixer {
    matrix: MixMatrix,
    in_channels: usize,
    output_planes: Vec<Vec<f64>>,
    in_frame: Vec<f64>,
    out_frame: Vec<f64>,
    // 仅测试需要
    #[cfg(test)]
    out_channels: usize,
}

impl ChannelMixer {
    pub fn new(source: ChannelLayout, dest: ChannelLayout, opts: MixOptions) -> Self {
        let matrix = MixMatrix::build(&source, &dest, opts);
        let in_channels = matrix.in_channels();
        let out_channels = matrix.out_channels();

        info!(
            "ChannelMixer: {} -> {} channels (identity={})",
            in_channels,
            out_channels,
            matrix.is_identity()
        );

        Self {
            matrix,
            in_channels,
            #[cfg(test)]
            out_channels,
            output_planes: (0..out_channels).map(|_| Vec::new()).collect(),
            in_frame: vec![0.0; in_channels],
            out_frame: vec![0.0; out_channels],
        }
    }

    pub fn needs_mixing(&self) -> bool {
        !self.matrix.is_identity()
    }

    #[cfg(test)]
    fn out_channels(&self) -> usize {
        self.out_channels
    }

    pub fn reset(&mut self) {
        for plane in &mut self.output_planes {
            plane.clear();
        }
    }

    /// 预分配输出平面，使稳态 `process` 调用永不分配。在管线搭建时调用一次，
    /// 传入单个周期能产出的最大帧数。
    pub fn ensure_output_capacity(&mut self, frames: usize) {
        for plane in &mut self.output_planes {
            if plane.capacity() < frames {
                plane.reserve(frames - plane.len());
            }
        }
    }

    /// 把按声道的平面输入混进输出平面，返回帧数。缺失的平面或缺失的样本按静音处理。
    pub fn mix(&mut self, input: &[Vec<f64>]) -> usize {
        let frames = (0..self.in_channels)
            .filter_map(|ch| input.get(ch).map(Vec::len))
            .min()
            .unwrap_or(0);

        for plane in &mut self.output_planes {
            plane.clear();
            plane.reserve(frames);
        }

        if input.len() >= self.in_channels {
            // 快路径（生产热路径）：平面齐全，行查找已提到样本循环外，直接下标取值
            for i in 0..frames {
                // in_frame 槽位数恰为 in_channels，zip 在此自然截断，不会读进多余平面
                for (slot, plane) in self.in_frame.iter_mut().zip(input) {
                    *slot = plane[i];
                }
                self.matrix.mix_frame(&self.in_frame, &mut self.out_frame);
                for (ch, &s) in self.out_frame.iter().enumerate() {
                    self.output_planes[ch].push(s);
                }
            }
        } else {
            // 慢路径（仅短平面输入）：缺失平面按静音，防短平面语义保留。
            // SmallVec 栈上内联（≤8 声道平面覆盖常见配置），防御性分支不付堆分配。
            let planes: SmallVec<[Option<&[f64]>; 8]> = (0..self.in_channels)
                .map(|ch| input.get(ch).map(|p| p.as_slice()))
                .collect();
            for i in 0..frames {
                for (ch, plane) in planes.iter().enumerate() {
                    self.in_frame[ch] = plane.map_or(0.0, |p| p[i]);
                }
                self.matrix.mix_frame(&self.in_frame, &mut self.out_frame);
                for (ch, &s) in self.out_frame.iter().enumerate() {
                    self.output_planes[ch].push(s);
                }
            }
        }

        frames
    }

    /// 上一次 `mix` 调用产出的平面。
    pub fn output_planes(&self) -> &[Vec<f64>] {
        &self.output_planes
    }

    /// 输出平面（可变借用），便于下游 DSP 原地处理。
    pub fn output_planes_mut(&mut self) -> &mut [Vec<f64>] {
        &mut self.output_planes
    }
}

/// 把标签归类到不丢声道数的最具体布局。
pub fn layout_from_labels(labels: Vec<ChannelLabel>) -> ChannelLayout {
    if labels.is_empty() {
        return ChannelLayout::None;
    }

    let mut pos = ChannelPosition::empty();
    let mut all_positioned = true;
    for label in &labels {
        match label {
            ChannelLabel::Positioned(p) => {
                if p.bits().is_power_of_two() && !pos.contains(*p) {
                    pos |= *p;
                } else {
                    all_positioned = false;
                    break;
                }
            }
            ChannelLabel::Discrete(_) => {
                all_positioned = false;
                break;
            }
        }
    }
    if all_positioned {
        ChannelLayout::Positioned(pos)
    } else {
        ChannelLayout::Custom(labels.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matrix_when_layouts_match() {
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&stereo, &stereo, MixOptions::default());
        assert!(m.is_identity());
        assert_eq!(m.in_channels(), 2);
        assert_eq!(m.out_channels(), 2);
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&mono, &stereo, MixOptions::default());
        let rows = m.rows();
        // L = FC, R = FC
        assert!((rows[0][0] - 1.0).abs() < 1e-9);
        assert!((rows[1][0] - 1.0).abs() < 1e-9);
        assert_eq!(m.in_channels(), 1);
        assert_eq!(m.out_channels(), 2);

        let out = mix_frame(&m, &[0.5]);
        assert!((out[0] - 0.5).abs() < 1e-9);
        assert!((out[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mono_reported_as_front_left_still_duplicates_to_stereo() {
        // 有些解码器把单声道源报成 FRONT_LEFT（`Position::from_count(1)` 给出的
        // 第一个 WAVE 位置位）而不是 FRONT_CENTER。混合器必须归一化这种情况，
        // 让单声道两只耳朵都出声，而不只是左耳。
        let mono_as_fl = ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT);
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&mono_as_fl, &stereo, MixOptions::default());
        let rows = m.rows();
        assert_eq!(m.in_channels(), 1);
        assert_eq!(m.out_channels(), 2);
        // L 与 R 都应拿到完整信号。
        assert!(
            (rows[0][0] - 1.0).abs() < 1e-9,
            "L coefficient was {}",
            rows[0][0]
        );
        assert!(
            (rows[1][0] - 1.0).abs() < 1e-9,
            "R coefficient was {}",
            rows[1][0]
        );

        let out = mix_frame(&m, &[0.7]);
        assert!((out[0] - 0.7).abs() < 1e-9);
        assert!((out[1] - 0.7).abs() < 1e-9);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let m = MixMatrix::build(&stereo, &mono, MixOptions::default());
        let rows = m.rows();
        // FC = 0.5*FL + 0.5*FR（前置对以满量折进中置，再除以 2）
        assert!((rows[0][0] - 0.5).abs() < 1e-9);
        assert!((rows[0][1] - 0.5).abs() < 1e-9);

        let out = mix_frame(&m, &[0.8, -0.4]);
        assert!((out[0] - 0.2).abs() < 1e-9);
    }

    #[test]
    fn five_point_one_to_stereo_atsc() {
        let src = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT,
        );
        let dst =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&src, &dst, MixOptions::default());
        let rows = m.rows();

        // 输入顺序（按位升序）：FL, FR, FC, LFE, RL, RR
        // L = FL + a*FC + a*RL            （LFE 默认丢弃）
        // R = FR + a*FC + a*RR
        // 然后整个矩阵乘以 n = 1/(1 + 2a)，使各行权重和为 1.0
        let a = SURROUND_ATTENUATION;
        let n = 1.0 / (1.0 + 2.0 * a);
        assert!((rows[0][0] - n).abs() < 1e-9); // FL
        assert!((rows[0][1] - 0.0).abs() < 1e-9); // FR
        assert!((rows[0][2] - a * n).abs() < 1e-9); // FC
        assert!((rows[0][3] - 0.0).abs() < 1e-9); // LFE
        assert!((rows[0][4] - a * n).abs() < 1e-9); // RL
        assert!((rows[0][5] - 0.0).abs() < 1e-9); // RR

        assert!((rows[1][0] - 0.0).abs() < 1e-9);
        assert!((rows[1][1] - n).abs() < 1e-9);
        assert!((rows[1][2] - a * n).abs() < 1e-9);
        assert!((rows[1][3] - 0.0).abs() < 1e-9);
        assert!((rows[1][4] - 0.0).abs() < 1e-9);
        assert!((rows[1][5] - a * n).abs() < 1e-9);
    }

    #[test]
    fn five_point_one_to_stereo_with_lfe() {
        let src = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT,
        );
        let dst =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let opts = MixOptions {
            lfe_to_stereo: true,
            ..MixOptions::default()
        };
        let m = MixMatrix::build(&src, &dst, opts);
        let rows = m.rows();
        let a = SURROUND_ATTENUATION;
        // LFE 在输入顺序里是下标 3；归一化后它相对 FL 仍保持同样的衰减比例。
        assert!((rows[0][3] / rows[0][0] - a).abs() < 1e-9);
        assert!((rows[1][3] / rows[1][1] - a).abs() < 1e-9);
    }

    #[test]
    fn seven_point_one_to_five_point_one_folds_sides() {
        // 7.1: FL FR FC LFE RL RR SL SR
        let src = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT
                | ChannelPosition::SIDE_LEFT
                | ChannelPosition::SIDE_RIGHT,
        );
        // 5.1: FL FR FC LFE RL RR
        let dst = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT,
        );
        let m = MixMatrix::build(&src, &dst, MixOptions::default());
        let rows = m.rows();
        let a = SURROUND_ATTENUATION;
        // 输出顺序：FL FR FC LFE RL RR
        // 输入顺序：FL FR FC LFE RL RR SL SR
        // RL/RR 两行的权重和为 1 + a，故矩阵按 n = 1/(1 + a) 归一化。
        let n = 1.0 / (1.0 + a);
        // SL（下标 6）以系数 a 折进 RL（输出下标 4）。
        assert!((rows[4][4] - n).abs() < 1e-9); // RL 恒等直通
        assert!((rows[4][6] - a * n).abs() < 1e-9); // SL -> RL
        // SR（下标 7）以系数 a 折进 RR（输出下标 5）。
        assert!((rows[5][5] - n).abs() < 1e-9);
        assert!((rows[5][7] - a * n).abs() < 1e-9);
    }

    #[test]
    fn mono_to_seven_point_one_routes_to_center() {
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let seven1 = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT
                | ChannelPosition::SIDE_LEFT
                | ChannelPosition::SIDE_RIGHT,
        );
        let m = MixMatrix::build(&mono, &seven1, MixOptions::default());
        let rows = m.rows();
        // 输入：FC。输出顺序：FL FR FC LFE RL RR SL SR
        assert!((rows[2][0] - 1.0).abs() < 1e-9); // FC 拿到单声道信号
        // 其余声道全部静音。
        for i in [0, 1, 3, 4, 5, 6, 7] {
            assert!((rows[i][0] - 0.0).abs() < 1e-9);
        }
    }

    #[test]
    fn stereo_to_seven_point_one_passes_through_front_pair() {
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let seven1 = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT
                | ChannelPosition::SIDE_LEFT
                | ChannelPosition::SIDE_RIGHT,
        );
        let m = MixMatrix::build(&stereo, &seven1, MixOptions::default());
        let rows = m.rows();
        // 输出顺序：FL FR FC LFE RL RR SL SR。输入：FL FR。
        assert!((rows[0][0] - 1.0).abs() < 1e-9);
        assert!((rows[1][1] - 1.0).abs() < 1e-9);
        for i in [2, 3, 4, 5, 6, 7] {
            assert!((rows[i][0] - 0.0).abs() < 1e-9);
            assert!((rows[i][1] - 0.0).abs() < 1e-9);
        }
    }

    #[test]
    fn count_based_mono_stereo_fallback() {
        let m = build_count_based(1, 2, MixOptions::default());
        assert!((m.rows()[0][0] - 1.0).abs() < 1e-9);
        assert!((m.rows()[1][0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn count_based_stereo_mono_fallback() {
        let m = build_count_based(2, 1, MixOptions::default());
        assert!((m.rows()[0][0] - 0.5).abs() < 1e-9);
        assert!((m.rows()[0][1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn count_based_identity() {
        let m = build_count_based(6, 6, MixOptions::default());
        assert!(m.is_identity());
    }

    #[test]
    fn count_based_zero_channels_does_not_underflow() {
        let no_output = build_count_based(2, 0, MixOptions::default());
        assert_eq!(no_output.in_channels(), 2);
        assert_eq!(no_output.out_channels(), 0);

        let no_input = build_count_based(0, 2, MixOptions::default());
        assert_eq!(no_input.in_channels(), 0);
        assert_eq!(no_input.out_channels(), 2);
    }

    #[test]
    fn normalization_prevents_clipping() {
        // 5.1 -> 立体声：L = FL + a*FC + a*RL 的权重和为 1 + 2a ≈ 2.414，故整个矩阵按比例
        // 缩小，让满幅输入恰好落在 1.0，而不是靠硬钳位。
        let src = ChannelLayout::Positioned(
            ChannelPosition::FRONT_LEFT
                | ChannelPosition::FRONT_RIGHT
                | ChannelPosition::FRONT_CENTER
                | ChannelPosition::LFE1
                | ChannelPosition::REAR_LEFT
                | ChannelPosition::REAR_RIGHT,
        );
        let dst =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&src, &dst, MixOptions::default());
        // 输入顺序：FL, FR, FC, LFE, RL, RR
        let out = mix_frame(&m, &[1.0, 1.0, 1.0, 0.0, 1.0, 1.0]);
        assert!(
            out[0] <= 1.0 && (out[0] - 1.0).abs() < 1e-9,
            "L was {}",
            out[0]
        );
        assert!(
            out[1] <= 1.0 && (out[1] - 1.0).abs() < 1e-9,
            "R was {}",
            out[1]
        );

        // 更轻的内容按同一系数衰减而非失真：前置对相对各折入声道保持原有平衡
        let a = SURROUND_ATTENUATION;
        let n = 1.0 / (1.0 + 2.0 * a);
        let quiet = mix_frame(&m, &[0.5, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!((quiet[0] - 0.5 * n).abs() < 1e-9, "L was {}", quiet[0]);
    }

    #[test]
    fn normalization_skips_matrices_that_cannot_clip() {
        // 单声道 -> 立体声以满增益复制；每行权重和本就是 1.0，因此矩阵必须原样不动
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let m = MixMatrix::build(&mono, &stereo, MixOptions::default());
        assert!((m.rows()[0][0] - 1.0).abs() < 1e-9);
        assert!((m.rows()[1][0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn custom_layout_falls_back_to_count() {
        let src = ChannelLayout::Custom(Box::new([
            ChannelLabel::Discrete(0),
            ChannelLabel::Discrete(1),
        ]));
        let dst = ChannelLayout::from_count_canonical(1);
        let m = MixMatrix::build(&src, &dst, MixOptions::default());

        assert!((m.rows()[0][0] - 0.5).abs() < 1e-9);
        assert!((m.rows()[0][1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn layout_from_labels_keeps_duplicate_positions_custom() {
        let layout = layout_from_labels(vec![
            ChannelLabel::Positioned(ChannelPosition::FRONT_LEFT),
            ChannelLabel::Positioned(ChannelPosition::FRONT_LEFT),
        ]);

        assert!(matches!(layout, ChannelLayout::Custom(_)));
        assert_eq!(layout.count(), 2);
    }

    #[test]
    fn position_index_works() {
        let mask = ChannelPosition::FRONT_LEFT
            | ChannelPosition::FRONT_RIGHT
            | ChannelPosition::FRONT_CENTER;
        assert_eq!(position_index(mask, ChannelPosition::FRONT_LEFT), Some(0));
        assert_eq!(position_index(mask, ChannelPosition::FRONT_RIGHT), Some(1));
        assert_eq!(position_index(mask, ChannelPosition::FRONT_CENTER), Some(2));
        assert_eq!(position_index(mask, ChannelPosition::LFE1), None);
    }

    fn mix_frame(m: &MixMatrix, input: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; m.out_channels()];
        m.mix_frame(input, &mut out);
        out
    }

    fn run_process(mixer: &mut ChannelMixer, input: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
        use crate::media::pipeline::ChannelBuffers;

        let frames = input.first().map(|v| v.len()).unwrap_or(0);
        let (mut producers, mut consumers) =
            ChannelBuffers::new(mixer.out_channels(), frames.max(1) * 2).split();

        let written = mixer.mix(&input);
        assert_eq!(written, frames);
        producers
            .write_vecs_nonblocking(mixer.output_planes())
            .unwrap();

        let read = consumers.try_read_to_staging(frames);
        assert_eq!(read, frames);
        consumers.staging().iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn process_mono_to_stereo_duplicates_plane() {
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let mut mixer = ChannelMixer::new(mono, stereo, MixOptions::default());
        assert!(mixer.needs_mixing());

        let out = run_process(&mut mixer, vec![vec![0.1, -0.2, 0.3]]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.1, -0.2, 0.3]);
        assert_eq!(out[1], vec![0.1, -0.2, 0.3]);
    }

    #[test]
    fn process_stereo_to_mono_averages() {
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let mut mixer = ChannelMixer::new(stereo, mono, MixOptions::default());

        let out = run_process(&mut mixer, vec![vec![0.8, 0.0], vec![-0.4, 1.0]]);
        assert_eq!(out.len(), 1);
        assert!((out[0][0] - 0.2).abs() < 1e-9);
        assert!((out[0][1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn process_empty_input_writes_nothing() {
        let stereo =
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT);
        let mono = ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER);
        let mut mixer = ChannelMixer::new(stereo, mono, MixOptions::default());

        assert_eq!(mixer.mix(&[vec![], vec![]]), 0);
        assert!(mixer.output_planes().iter().all(Vec::is_empty));
    }
}
