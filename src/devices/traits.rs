use crate::media::pipeline::ChannelConsumers;

use super::{
    errors::{
        CloseError, FindError, InfoError, OpenError, ResetError, StateError, SubmissionError,
    },
    format::FormatInfo,
};

/// DeviceProvider 定义与设备提供方交互所需的方法。设备提供方负责给出系统可用
/// 设备列表，并在这些设备上开关流。
pub trait DeviceProvider {
    /// 要求设备提供方完成自身初始化。
    fn initialize(&mut self);
    /// 返回该设备提供方的默认设备。
    fn get_default_device(&mut self) -> Result<Box<dyn Device>, FindError>;
}

pub trait Device {
    /// 要求设备以给定格式打开一个流。
    fn open_device(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError>;

    /// 返回设备的默认格式。
    fn get_default_format(&self) -> Result<FormatInfo, InfoError>;
    /// 返回设备名称。
    fn get_name(&self) -> Result<String, InfoError>;
}

pub trait OutputStream {
    /// 关闭流并释放与之关联的一切资源。
    fn close_stream(&mut self) -> Result<(), CloseError>;
    /// 让设备开始播放音频。
    fn play(&mut self) -> Result<(), StateError>;
    /// 让设备停止播放音频。部分提供方可能根本不会真的停下——本函数可以是空操作。
    /// 调用本函数后未调 `play` 就继续提交帧属未定义行为，可能让线程无限阻塞。
    ///
    /// 实现时设备绝不能丢弃已提交的音频数据：在「丢数据」与「本函数空操作」之间，
    /// 必须选空操作。
    fn pause(&mut self) -> Result<(), StateError>;
    /// 推进流的延后工作；播放线程主循环每轮都调用一次。让 `pause()` 得以立即返回，
    /// 其淡出留到此处稍后完成。默认：空操作。
    fn poll(&mut self) -> Result<(), StateError> {
        Ok(())
    }
    /// 是否有延后工作待办、因此 `poll()` 很快要再跑一次（例如等待到点才停设备的
    /// pause 淡出）。返回 true 期间播放线程保持它的短空闲节奏，转为 false 才挂起在
    /// 命令通道上。默认：无。
    fn has_deferred_work(&self) -> bool {
        false
    }
    /// 让设备重置缓冲区。pause 之后重启播放时用它来避免播出陈旧数据（例如用户在
    /// seek 或切歌前先按了暂停）。
    fn reset(&mut self) -> Result<(), ResetError>;
    /// 让设备把音量设为给定值，取值应在 0.0 到 1.0 之间。部分设备提供方不支持硬件
    /// 或系统级音量控制，改为在提交给设备之前用该值调整音频数据本身的音量。
    fn set_volume(&mut self, volume: f64) -> Result<(), StateError>;

    /// 设置 ReplayGain 乘数，叠加在音量之上。
    fn set_replaygain(&mut self, _gain: f64) -> Result<(), StateError> {
        Ok(())
    }

    /// 从环形缓冲消费者取出样本并提交给设备。
    fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, SubmissionError>;
}
