use crate::devices::dummy;
use crate::test_support::TestDir;

use super::harness::{
    configure_bounded_device, configure_dummy_device, engine_lock, engine_playing, run_to_eof,
    write_wav_i16,
};

const DEVICE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const MAX_CYCLES: usize = 100_000;

/// `signal_with_tail_marker` 在末尾附加的常量标记长度。
const TAIL_MARKER_LEN: usize = 1024;

/// 在采集结果尾部搜寻曲目结束标记时检查的帧数。
const TAIL_WINDOW: usize = 4096;

/// 尾部标记写在 ±`i16::MAX / 2`（设备层缩放到 [-1, 1] 后约 ±0.5）处；
/// 只有越过此阈值才认定尾部存在。
const MARKER_THRESHOLD: f64 = 0.4;

/// 接缝处仍可判为无缝切歌的平线偏移上限，按缩放后的采样值计。
const SEAM_DROPOUT_TOLERANCE: f64 = 2_000.0;

/// 以常量标记收尾的信号，使尾部被截断时可被检出。
fn signal_with_tail_marker(frames: usize, marker: i16) -> Vec<i16> {
    (0..frames * CHANNELS as usize)
        .map(|i| {
            let frame = i / CHANNELS as usize;
            if frame >= frames - TAIL_MARKER_LEN {
                marker
            } else {
                // 低幅度的斜坡主体
                ((frame % 1000) as i16) - 500
            }
        })
        .collect()
}

fn expected_min_frames(source_frames: usize, source_rate: u32) -> usize {
    (source_frames as f64 * f64::from(DEVICE_RATE) / f64::from(source_rate)).ceil() as usize + 16
}

fn constant_signal(frames: usize, value: i16) -> Vec<i16> {
    super::harness::constant_signal(frames, CHANNELS as usize, value)
}

/// 两条曲目都要完整落到设备上，且采集尾部必须出现曲目 B 的负极结束标记。
fn assert_both_tracks_reached_device(planes: &[Vec<f64>], expected_min: usize) {
    for (ch, plane) in planes.iter().enumerate() {
        assert!(
            plane.len() >= expected_min,
            "channel {ch}: only {} frames reached the device across both \
             tracks, expected at least {expected_min}",
            plane.len()
        );
        let tail = &plane[plane.len().saturating_sub(TAIL_WINDOW)..];
        let trough = tail.iter().fold(0.0_f64, |acc, &s| acc.min(s));
        assert!(
            trough < -MARKER_THRESHOLD,
            "channel {ch}: second track's tail missing (trough {trough})"
        );
    }
}

#[test]
fn gapless_transition_under_backpressure_drops_no_frames() {
    let _guard = engine_lock();
    let rate = 44_100;
    configure_dummy_device(rate, "S16", CHANNELS);
    // 用小环形缓冲模拟背压，此时管线一帧都不许丢
    configure_bounded_device(4096, 512);

    let dir = TestDir::new("hb-eof-drain-backpressure");
    let frames_a = 40_000;
    let frames_b = 40_000;
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    write_wav_i16(&path_a, rate, CHANNELS, &constant_signal(frames_a, 12_000));
    write_wav_i16(&path_b, rate, CHANNELS, &constant_signal(frames_b, 12_000));

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path_a);
    run_to_eof(&mut engine, MAX_CYCLES);
    // 设备环形缓冲里仍是 A 的数据时就无缝切入曲目 B
    engine
        .open(&path_b, true, None)
        .expect("failed to open the second track");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    let expected = frames_a + frames_b;
    for (ch, plane) in planes.iter().enumerate() {
        assert_eq!(
            plane.len(),
            expected,
            "channel {ch}: {} frames reached the device, source has {expected} \
             — {} frames were dropped at the gapless transition under \
             back-pressure",
            plane.len(),
            expected as i64 - plane.len() as i64,
        );
    }
}

#[test]
fn gapless_transition_has_no_seam_dropout() {
    let _guard = engine_lock();
    // 重采样在生效（44.1k 源 -> 48k 设备），所以必须跨曲目边界延续的是重采样器状态
    configure_dummy_device(DEVICE_RATE, "S16", CHANNELS);

    let dir = TestDir::new("hb-eof-drain-gapless-continuity");
    let source_rate = 44_100;
    let frames_a = 33_100;
    let frames_b = 40_000;
    let value = 12_000_i16;
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    // 两条曲目是同一个常量直流，接缝无缝即表现为一条平线
    write_wav_i16(
        &path_a,
        source_rate,
        CHANNELS,
        &constant_signal(frames_a, value),
    );
    write_wav_i16(
        &path_b,
        source_rate,
        CHANNELS,
        &constant_signal(frames_b, value),
    );

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path_a);
    run_to_eof(&mut engine, MAX_CYCLES);
    engine
        .open(&path_b, true, None)
        .expect("failed to open the second track");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    assert_eq!(planes.len(), CHANNELS as usize);

    // 确认输出在接缝前后保持恒定
    let expected = f64::from(value);
    let guard = 8_192;

    for (ch, plane) in planes.iter().enumerate() {
        assert!(
            plane.len() > 2 * guard,
            "channel {ch}: only {} frames captured",
            plane.len()
        );

        let interior_end = plane.len() - guard;
        for (frame, &sample) in plane.iter().enumerate().take(interior_end).skip(guard) {
            // sample 是由 i16 缩放来的 [-1, 1] 区间 f64
            let scaled = sample * f64::from(i16::MAX);
            assert!(
                (scaled - expected).abs() < SEAM_DROPOUT_TOLERANCE,
                "channel {ch}: dropout at frame {frame} (value {scaled:.0}, expected \
                ~{expected:.0}), not gapless",
            );
        }
    }
}

#[test]
fn resampled_track_tail_reaches_device_at_end_of_playback() {
    let _guard = engine_lock();
    configure_dummy_device(DEVICE_RATE, "S16", CHANNELS);

    let dir = TestDir::new("hb-eof-drain-tail");
    let path = dir.join("source.wav");
    let source_rate = 44_100;
    // 帧数不是块长的整数倍，理应让重采样器里残留数据
    let frames = 33_100;
    write_wav_i16(
        &path,
        source_rate,
        CHANNELS,
        &signal_with_tail_marker(frames, i16::MAX / 2),
    );

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path);
    run_to_eof(&mut engine, MAX_CYCLES);

    // 应把重采样器剩余尾部冲刷到设备
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    assert_eq!(planes.len(), CHANNELS as usize);
    let expected_min = expected_min_frames(frames, source_rate);
    for (ch, plane) in planes.iter().enumerate() {
        assert!(
            plane.len() >= expected_min,
            "channel {ch}: only {} frames reached the device, expected at \
             least {expected_min} — the track tail was truncated",
            plane.len()
        );

        // 找曲目结束标记
        let tail = &plane[plane.len().saturating_sub(TAIL_WINDOW)..];
        let peak = tail.iter().fold(0.0_f64, |acc, &s| acc.max(s));
        assert!(
            peak > MARKER_THRESHOLD,
            "channel {ch}: end-of-track marker missing from the device \
             stream tail (peak {peak})"
        );
    }
}

#[test]
fn gapless_same_rate_tracks_lose_no_frames() {
    let _guard = engine_lock();
    configure_dummy_device(DEVICE_RATE, "S16", CHANNELS);

    let dir = TestDir::new("hb-eof-drain-gapless");
    let source_rate = 44_100;
    let frames_a = 33_100;
    let frames_b = 21_500;
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    write_wav_i16(
        &path_a,
        source_rate,
        CHANNELS,
        &signal_with_tail_marker(frames_a, i16::MAX / 2),
    );
    write_wav_i16(
        &path_b,
        source_rate,
        CHANNELS,
        &signal_with_tail_marker(frames_b, i16::MIN / 2),
    );

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path_a);
    run_to_eof(&mut engine, MAX_CYCLES);
    // 无缝切歌：重采样器（连同其尾部数据）延续下来
    engine
        .open(&path_b, true, None)
        .expect("failed to open the second track");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    let expected_min = expected_min_frames(frames_a + frames_b, source_rate);
    assert_both_tracks_reached_device(&planes, expected_min);
}

#[test]
fn rate_change_between_tracks_flushes_previous_tail() {
    let _guard = engine_lock();
    configure_dummy_device(DEVICE_RATE, "S16", CHANNELS);

    let dir = TestDir::new("hb-eof-drain-rate-change");
    let rate_a = 44_100;
    let rate_b = 32_000;
    let frames_a = 33_100;
    let frames_b = 21_500;
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    write_wav_i16(
        &path_a,
        rate_a,
        CHANNELS,
        &signal_with_tail_marker(frames_a, i16::MAX / 2),
    );
    write_wav_i16(
        &path_b,
        rate_b,
        CHANNELS,
        &signal_with_tail_marker(frames_b, i16::MIN / 2),
    );

    let capture = dummy::install_capture();
    let mut engine = engine_playing(&path_a);
    run_to_eof(&mut engine, MAX_CYCLES);
    // 已请求保留状态，但采样率变化必然重建 —— 旧重采样器的尾部
    // 必须冲刷出去，不能丢弃
    engine
        .open(&path_b, true, None)
        .expect("failed to open the second track");
    run_to_eof(&mut engine, MAX_CYCLES);
    engine.stop();
    dummy::uninstall_capture();

    let planes = capture.lock().unwrap();
    let expected_min =
        expected_min_frames(frames_a, rate_a) + expected_min_frames(frames_b, rate_b);
    assert_both_tracks_reached_device(&planes, expected_min);
}
