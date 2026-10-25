//! 设备格式漂移（流审计 #11）：默认设备热插拔后 `create_stream` 的请求叠加在
//! 新设备默认格式之上——成功建流 ≠ 格式没漂。修复（recreate_stream 内的
//! 漂移检测 + 管线重建）前：管线 device_input 生产者声道数、重采样器目标率、
//! 混音器布局都停在旧格式，consume_from 按管线声道数交织写进新声道布局的
//! 设备环（静默垃圾音频：声道错拆 / 变调变速）。

use crate::devices::{
    dummy,
    errors::SubmissionError,
    format::{BufferSize, ChannelSpec, FormatInfo, SampleFormat},
    traits::Device,
};
use crate::media::pipeline::ChannelBuffers;
use crate::playback::events::PlaybackEvent;
use crate::test_support::TestDir;

use super::harness::{
    configure_device_death, configure_dummy_device, configure_hotplug_format, constant_signal,
    engine_lock, engine_playing_with_events, run_to_eof, write_wav_i16,
};

const CHANNELS: u16 = 2;
const MAX_CYCLES: usize = 100_000;

/// T1：率漂移·成功路径。设备 48k → 热插拔后新默认 44.1k（请求仍带旧 2ch，被
/// 直接接受）。PCM wav 每包 1152 帧（symphonia riff `MAX_FRAMES_PER_PACKET`）。
///
/// 预期捕获帧数：die(20_000) 的死点落在 20_736 帧处（首个 ≥ 阈值的包边界），
/// 死点当轮 device_input 里的一包（1152 帧）随管线重建丢弃，其余 122_112 帧
/// 经 48k→44.1k 重采样（×0.91875）→ ≈132_926。窗口 [0.88N, 0.945N] 下界排除
/// 「丢太多」，上界排除「重采样器没按新率重建」——预修复是 48k 全直通的
/// 144_000 帧（> 0.945N），红灯即钉住这一点。
#[test]
fn device_rate_drift_on_recreate_rebuilds_pipeline() {
    let _guard = engine_lock();
    let rate = 48_000;
    configure_dummy_device(rate, "S16", CHANNELS);

    let dir = TestDir::new("hb-device-drift-rate");
    let frames = 144_000;
    let path = dir.join("a.wav");
    write_wav_i16(
        &path,
        rate,
        CHANNELS,
        &constant_signal(frames, CHANNELS as usize, 9_000),
    );

    let capture = dummy::install_capture();
    // die_after 只在 open_device 时读一次 env：死点要钉在首条流上，必须先于建引擎
    // 设置（同 error_handling.rs 的既有次序），否则首条流 die_after=None 永不死。
    configure_device_death(20_000);
    let (mut engine, mut events) = engine_playing_with_events(&path);
    // 模拟热插拔：新默认设备 44.1k。请求 = 旧 2ch + 新默认率 → 成功路径漂移。
    // 只需赶在死点触发的 create_stream 之前生效即可。
    configure_hotplug_format(44_100, CHANNELS);
    // run_to_eof 对 FatalError 直接 panic（断言 (c) 内含）。
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let min_frames = (frames as f64 * 0.88) as usize;
    let max_frames = (frames as f64 * 0.945) as usize;
    let planes = capture.lock().unwrap();
    assert_eq!(planes.len(), CHANNELS as usize, "expected a stereo capture");
    for (ch, plane) in planes.iter().enumerate() {
        assert!(
            plane.len() >= min_frames && plane.len() <= max_frames,
            "channel {ch}: {} frames captured, expected [{min_frames}, {max_frames}] \
             (rate-drifted pipeline must resample 48k -> 44.1k and drop only the \
             in-flight frames at the death point; unresampled passthrough yields \
             the full {frames})",
            plane.len()
        );
    }

    // 重建后 sync_eq_format 以管线真实产出率上报 UI。
    let mut saw_new_rate = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, PlaybackEvent::SampleRateChanged(44_100)) {
            saw_new_rate = true;
        }
    }
    assert!(
        saw_new_rate,
        "SampleRateChanged(44_100) not observed after the drift rebuild"
    );
}

/// T2：声道漂移·回退分支。设备 44.1k/2ch → 热插拔后新默认单声道，且
/// `HB_DUMMY_STRICT_FORMAT` 让 2ch 请求被拒，DeviceController 回退到设备默认
/// 格式开流（1ch）——流审计里最完整的漂移变体。
///
/// 死点取 0（首次 consume 即死）：capture 的 planes 只增不减，若死点前已有
/// 立体声捕获，修复后的 planes.len() 也会是 2——把死点挪到首帧之前，plane
/// 数才是「管线是否按 1ch 重建」的直接判据（规格断言 (a) 的字面形态）。
/// 预修复：旧管线 2ch 平面照常提交进 1ch 流 → 2 planes（红灯）。
#[test]
fn device_channel_drift_fallback_rebuilds_pipeline() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);

    let dir = TestDir::new("hb-device-drift-channels");
    let frames = 40_000;
    let path = dir.join("a.wav");
    write_wav_i16(
        &path,
        rate,
        CHANNELS,
        &constant_signal(frames, CHANNELS as usize, 9_000),
    );

    let capture = dummy::install_capture();
    // 同 T1：die_after 在 open_device 读一次，死点（首次 consume 即死）必须钉在
    // 首条流上——先于建引擎设置。
    configure_device_death(0);
    let (mut engine, _events) = engine_playing_with_events(&path);
    configure_hotplug_format(rate, 1);
    unsafe {
        std::env::set_var("HB_DUMMY_STRICT_FORMAT", "1");
    }
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let min_frames = (frames as f64 * 0.9) as usize;
    let planes = capture.lock().unwrap();
    assert_eq!(
        planes.len(),
        1,
        "expected a mono capture after the fallback rebuild (pre-fix: 2 — the stale \
         stereo pipeline kept feeding the mono stream)"
    );
    assert!(
        planes[0].len() >= min_frames && planes[0].len() <= frames,
        "channel 0: {} frames captured, expected [{min_frames}, {frames}] \
         (2 -> 1 downmix must not drop frames; only the two in-flight packets \
         at the death point are discarded)",
        planes[0].len()
    );
}

/// T3：devices 层守卫单测——流按 open 时的声道数消费，管线平面数不一致即
/// 显式拒绝（cpal 侧同构守卫无真实设备可测，由本测试代表）。
#[test]
fn dummy_stream_rejects_mismatched_channel_count() {
    let _guard = engine_lock();
    // 清理上个测试可能遗留的 knob（BOUNDED/STRICT/DIE 等），保证走无界分支。
    configure_dummy_device(44_100, "S16", 2);

    let mut device = dummy::DummyDevice {};
    let mut stream = device
        .open_device(FormatInfo {
            originating_provider: "dummy",
            sample_type: SampleFormat::Signed16,
            sample_rate: 44_100,
            buffer_size: BufferSize::Fixed(4096),
            channels: ChannelSpec::Count(2),
        })
        .expect("opening the dummy device with 2 channels must succeed");

    // 3 声道管线喂 2 声道流 → 显式拒绝而不是错拆样本。
    let (_, mut mismatched) = ChannelBuffers::new(3, 64).split();
    assert_eq!(
        stream.consume_from(&mut mismatched),
        Err(SubmissionError::FormatMismatch {
            stream_channels: 2,
            pipeline_channels: 3,
        })
    );

    // 匹配声道数 → 正常提交路径（空环零帧）。
    let (_, mut matched) = ChannelBuffers::new(2, 64).split();
    assert_eq!(stream.consume_from(&mut matched), Ok(0));
}
