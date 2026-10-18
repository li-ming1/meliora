use serde::{Deserialize, Serialize};

use crate::{
    media::metadata::Metadata,
    settings::{equalizer::EqualizerSettings, playback::PlaybackSettings},
};

use super::{queue::QueueItemData, thread::PlaybackState};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Clone, PartialEq, Copy, Serialize, Deserialize)]
pub enum RepeatState {
    NotRepeating,
    Repeating,
    RepeatingOne,
}

/// 发往播放线程的命令，供其它线程控制播放线程。播放线程从 MPSC 通道收取这些
/// 命令并按到达顺序处理：每次唤醒立即处理；停止时挂在这条通道上，直到有命令
/// 或延后的设备任务到达。
#[derive(Debug, PartialEq, Clone)]
pub enum PlaybackCommand {
    /// 请求播放线程开始播放。
    Play,
    /// 请求播放线程暂停播放。
    Pause,
    /// 正在播放就暂停、否则就开始播放。仅 macOS 远程控制与 Linux MPRIS
    /// 控制器会构造它。
    #[cfg(not(target_os = "windows"))]
    TogglePlayPause,
    /// 请求播放线程把指定文件排在当前文件之后播放。若无正在播放的文件，
    /// 该文件立即播放。
    Queue(QueueItemData),
    /// 请求播放线程把一批文件依次排在当前文件之后播放。若无正在播放的文件，
    /// 列表第一个文件立即播放。
    QueueList(Vec<QueueItemData>),
    /// 请求播放线程把指定文件插入队列的给定位置。位置超出队列长度时追加到末尾。
    InsertAt {
        item: QueueItemData,
        position: usize,
    },
    /// 请求播放线程把一批文件插入队列的给定位置。位置超出队列长度时依次追加到
    /// 末尾。
    InsertListAt {
        items: Vec<QueueItemData>,
        position: usize,
    },
    /// 请求播放线程跳到队列中的下一个文件。
    Next,
    /// 请求播放线程跳到队列中的上一个文件。
    /// 当前文件已播放超过 5 秒时改为从头重播。
    Previous,
    /// 请求播放线程清空队列。
    ClearQueue,
    /// 跳到队列中的指定位置。
    Jump(usize),
    /// 请求播放线程在当前文件中跳转定位到指定位置。
    Seek(f64),
    /// 请求播放线程把音量设为指定值。
    SetVolume(f64),
    /// 请求播放线程用指定队列替换当前队列。这会把正在播放的曲目设为队列第一项。
    ReplaceQueue(Vec<QueueItemData>),
    /// 请求播放线程停止播放。仅 Windows SMTC 与 Linux MPRIS 控制器会构造它。
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Stop,
    /// 切换"当前曲目播完后是否停止播放"。
    StopAfterCurrent,
    /// 请求播放线程打乱（或停止打乱）队列中后续的曲目。注意当前实现会复制
    /// *整个* 队列。
    ToggleShuffle,
    /// 请求播放线程把随机播放模式设为指定值。
    SetShuffle(bool),
    /// 请求把循环播放设置设为指定的 RepeatState。
    SetRepeat(RepeatState),
    /// 请求把给定索引处的条目从队列移除。
    RemoveItem(usize),
    /// 请求把给定索引列表处的条目从队列移除。
    RemoveItems(Vec<usize>),
    /// 请求把某个条目在队列中从一个位置移到另一个位置。
    /// 第一个 usize 是源索引，第二个是目标索引。
    MoveItem { from: usize, to: usize },
    /// 请求把多个条目移到队列中的同一个目标位置。源索引按升序排列；目标是
    /// 全部条目移动完成后的最终位置。
    MoveItems { indices: Vec<usize>, to: usize },
    /// 请求撤销最近一次队列变更。
    Undo,
    /// 告知播放线程播放设置已变更。
    SettingsChanged(PlaybackSettings),
    /// 请求播放线程实时应用新的均衡器设置。与 `SettingsChanged` 不同，这里
    /// 不落盘持久化。
    SetEqualizer(EqualizerSettings),
    /// 告知播放线程应用窗口当前是否获得焦点。
    SetPositionBroadcastActive(bool),
    /// 请求播放线程用指定队列替换当前队列。与 ReplaceQueue 不同，播放线程会
    /// 跳到新队列中指定的索引，而不是第一项。
    ReplaceQueueWithIndex(Vec<QueueItemData>, usize),
}

/// 来自播放线程的事件：把播放线程的信息传达到其它线程。播放线程把这些事件发
/// 到一个 MPSC 通道，主线程按到达顺序处理。
#[derive(Debug, PartialEq, Clone)]
pub enum PlaybackEvent {
    /// 播放状态已变更。
    StateChanged(PlaybackState),
    /// 当前文件已变更，携带新文件的路径。
    SongChanged(PathBuf),
    /// 当前文件的时长已变更。u64 是新时长，单位毫秒。
    DurationChanged(u64),
    /// 队列已更新。
    QueueUpdated,
    /// 队列中的位置已变更。usize 是新位置。
    QueuePositionChanged(usize),
    /// MediaProvider 给出了供 UI 消费的新元数据。Metadata 装箱以避免枚举体积
    /// 膨胀。
    MetadataUpdate(Box<Metadata>),
    /// MediaProvider 给出了供 UI 消费的新专辑封面图。用 `Arc` 让字节在多个
    /// models 之间共享，而不是每个处理者各拷一份。
    AlbumArtUpdate(Option<Arc<[u8]>>),
    /// 当前文件的播放位置已变更，单位毫秒。
    PositionChanged(u64),
    /// 线程内随机播放被开启或关闭时的通知。
    ShuffleToggled(bool),
    /// 循环播放状态已变更。
    RepeatChanged(RepeatState),
    /// 音量已变更。f64 是新音量，取值 0.0 到 1.0。
    VolumeChanged(f64),
    /// "播完当前曲目即停止"被置位或清除。
    StopAfterCurrentChanged(bool),
    /// 输出流的采样率已变更。
    SampleRateChanged(u32),
}
