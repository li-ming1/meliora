use crate::devices::channels::ChannelLayout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Float64,
    Float32,
    Signed32,
    Unsigned32,
    Signed24,
    Unsigned24,
    Signed16,
    Unsigned16,
    Signed8,
    Unsigned8,
}

/// 流所用声道的规格说明。
///
/// 拿不到位置信息时用 `Count`（例如只上报声道数的 cpal 设备，或不暴露
/// 布局的解码器）。内置声道混合器会经 [`ChannelLayout::from_count_canonical`]
/// 把 `Count` 解析成规范的位置布局。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChannelSpec {
    /// 只有裸声道数，无位置信息。
    Count(u16),
    /// 显式声道布局。
    Layout(ChannelLayout),
}

impl ChannelSpec {
    /// 声道总数。
    pub fn count(&self) -> u16 {
        match self {
            ChannelSpec::Count(count) => *count,
            ChannelSpec::Layout(layout) => layout.count() as u16,
        }
    }

    /// 解析成位置式 [`ChannelLayout`]；裸声道数回退到规范映射。
    pub fn to_layout(&self) -> ChannelLayout {
        match self {
            ChannelSpec::Count(count) => ChannelLayout::from_count_canonical(*count),
            ChannelSpec::Layout(layout) => layout.clone(),
        }
    }
}

impl From<u16> for ChannelSpec {
    fn from(count: u16) -> Self {
        ChannelSpec::Count(count)
    }
}

impl From<ChannelLayout> for ChannelSpec {
    fn from(layout: ChannelLayout) -> Self {
        ChannelSpec::Layout(layout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferSize {
    /// 支持的缓冲区大小闭区间。
    Range(u32, u32),
    Fixed(u32),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatInfo {
    pub originating_provider: &'static str,
    pub sample_type: SampleFormat,
    pub sample_rate: u32,
    pub buffer_size: BufferSize,
    pub channels: ChannelSpec,
}
