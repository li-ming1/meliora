use tokio::sync::mpsc::unbounded_channel;

use crate::devices::dummy;
use crate::playback::dsp::spectrum::spectrum_tap;
use crate::playback::thread::audio_engine::{AudioEngine, EngineCycleResult, EngineState};
use crate::test_support::{TestDir, register_test_media_providers};

use super::harness::{
    configure_bounded_device, configure_device_death, configure_dummy_device, engine_lock,
    engine_playing, run_to_eof, write_wav_i16,
};

const CHANNELS: u16 = 2;
const MAX_CYCLES: usize = 100_000;

fn constant_signal(frames: usize, value: i16) -> Vec<i16> {
    super::harness::constant_signal(frames, CHANNELS as usize, value)
}

#[test]
fn device_death_mid_playback_recovers_without_dropping_frames() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);
    // 让设备在曲目中途失败一次。
    configure_device_death(5_000);

    let dir = TestDir::new("hb-robustness-device-death");
    let frames = 20_000;
    let path = dir.join("a.wav");
    write_wav_i16(&path, rate, CHANNELS, &constant_signal(frames, 9_000));

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path);
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    for (ch, plane) in planes.iter().enumerate() {
        assert_eq!(
            plane.len(),
            frames,
            "channel {ch}: {} frames reached the device, source has {frames} — frames were \
             dropped when the device faulted",
            plane.len()
        );
    }
}

#[test]
fn seek_while_playing_flushes_and_continues() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);

    let dir = TestDir::new("hb-robustness-seek");
    let frames = 40_000;
    let path = dir.join("a.wav");
    write_wav_i16(&path, rate, CHANNELS, &constant_signal(frames, 8_000));

    let mut engine = engine_playing(&path);

    // 先播放一段，让缓冲区里确实有待冲刷的音频。
    for _ in 0..50 {
        engine.process_cycle();
    }

    // 播放中跳转定位必须冲刷设备与管线缓冲并继续播到 EOF，
    // 而不是推迟复位或挂住。
    engine.seek(0.0).expect("seek while playing failed");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
}

#[test]
fn playback_continues_after_earlier_device_fault() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);
    configure_device_death(2_000);

    let dir = TestDir::new("hb-robustness-after-fault");
    let frames = 8_000;
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    write_wav_i16(&path_a, rate, CHANNELS, &constant_signal(frames, 7_000));
    write_wav_i16(&path_b, rate, CHANNELS, &constant_signal(frames, -7_000));

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path_a);
    run_to_eof(&mut engine, MAX_CYCLES); // 故障在 A 曲播放期间触发
    engine
        .open(&path_b, false, None)
        .expect("failed to open track B");
    run_to_eof(&mut engine, MAX_CYCLES); // B 曲落在一条健康的流上
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    let expected = frames * 2;
    for (ch, plane) in planes.iter().enumerate() {
        assert_eq!(
            plane.len(),
            expected,
            "channel {ch}: {} frames reached the device across both tracks, expected {expected}",
            plane.len()
        );
    }
}

/// F3 契约：open 失败统一落地为「Playing + 无媒体 + 无管线」——线程侧的
/// 50 周期无进展机制因此可观测、可推进（落 Idle 会让 idle_wait 挂起、UI
/// 永久停留在旧曲播放态）。
#[test]
fn failed_open_keeps_engine_observable_and_advancing() {
    let _guard = engine_lock();
    configure_dummy_device(44_100, "S16", CHANNELS);
    register_test_media_providers();

    let dir = TestDir::new("hb-robustness-failed-open");
    let missing = dir.join("missing.wav");

    // 手搓引擎（同 harness::engine_playing 形态，但 open 预期失败）。
    let mut engine = AudioEngine::new(unbounded_channel().0, spectrum_tap().0);
    engine.initialize().expect("engine initialization failed");

    let result = engine.open(&missing, false, None);
    assert!(result.is_err(), "opening a missing file must fail");
    assert_eq!(engine.state(), EngineState::Playing);
    // 媒体缺失门保持 NothingToDo（不是 DeviceBackoff）：设备有流，无产出
    // 归因于曲料，由无进展计数推进跳曲。
    assert_eq!(engine.process_cycle(), EngineCycleResult::NothingToDo);
    engine.stop();
}

/// F4 契约：设备重建退避窗内的静默无产出返回 DeviceBackoff（修复前是
/// NothingToDo，即被 50 周期无进展误计数的元凶）；窗外首次失败仍走
/// FatalError 旧语义。
#[test]
fn backoff_window_cycle_is_device_backoff_not_no_progress() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);
    // 硬故障 knob：每个 consume 周期都失败，把引擎逼进重建退避窗
    // （DIE_AFTER 是一次性闩锁，到不了这条路径）。
    unsafe {
        std::env::set_var("HB_DUMMY_PERSISTENT_FAULT", "1");
    }

    let dir = TestDir::new("hb-robustness-backoff-window");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    write_wav_i16(&path_a, rate, CHANNELS, &constant_signal(4_000, 7_000));
    write_wav_i16(&path_b, rate, CHANNELS, &constant_signal(4_000, -7_000));

    let mut engine = engine_playing(&path_a);
    // 首次失败在退避窗外：warn → 重建（dummy open_device 成功）→ 重试仍失败
    // → FatalError（旧语义保留；2s 退避窗在重建前已立起）。
    let first = engine.process_cycle();
    // 窗口活跃（2s）内打开 B 成功后：consume 的失败走窗口内早退。env 在取值
    // 之后、断言之前清理，任何断言 panic 都不遗留污染。
    engine.open(&path_b, false, None).expect("open B succeeded");
    let in_window = engine.process_cycle();
    unsafe {
        std::env::remove_var("HB_DUMMY_PERSISTENT_FAULT");
    }
    engine.stop();

    assert!(
        matches!(first, EngineCycleResult::FatalError(_)),
        "first fault is outside the backoff window: {first:?}"
    );
    assert_eq!(in_window, EngineCycleResult::DeviceBackoff);
}

/// L-seek：暂停中跳转定位后恢复，不得先播出管线里缓冲的跳转前旧帧（错位
/// 音频）。前半 +8000（≈2.27 s @44.1 kHz）、后半 -8000 且加长到 seek(5.0)
/// 稳落负值区、30 个周期也远不触 EOF；恢复边界后第一个「设备环容量+余量」
/// 窗口只允许前半正值帧——修复前管线旧负值帧先流出约一圈（4096 帧级）即失败。
#[test]
fn paused_seek_does_not_replay_stale_pipeline_frames() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);
    // 无界 dummy 每次 consume 排空全部，管线不积压，复现必须背压。
    configure_bounded_device(4096, 512);

    let dir = TestDir::new("hb-robustness-paused-seek");
    let mut samples = constant_signal(100_000, 8_000);
    samples.extend_from_slice(&constant_signal(400_000, -8_000));
    let path = dir.join("a.wav");
    write_wav_i16(&path, rate, CHANNELS, &samples);

    let mut engine = engine_playing(&path);
    // Playing 态跳进后半负值区，跑 30 个周期让管线/设备环积压旧负值帧。
    engine.seek(5.0).expect("seek into tail failed");
    for _ in 0..30 {
        engine.process_cycle();
    }

    // 此刻起开始记录：恢复边界之后的帧只允许来自跳转目标（前半正值）。
    let capture = dummy::install_capture();
    let l0 = {
        let planes = capture.lock().unwrap();
        planes.first().map_or(0, |plane| plane.len())
    };
    engine.pause().expect("pause failed");
    // 修复点：Paused 态 seek 冲刷管线各级（缓冲的是跳转前的负值帧）。
    engine.seek(0.0).expect("paused seek failed");
    engine.play().expect("resume failed");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    let window_end = (l0 + 4096 + 1024).min(planes[0].len());
    for (ch, plane) in planes.iter().enumerate() {
        assert!(
            plane[l0..window_end].iter().all(|s| *s > 0.0),
            "channel {ch}: stale pre-seek frames replayed after resume — first \
             {} samples after the resume boundary contain non-positive samples",
            window_end - l0
        );
    }
}
