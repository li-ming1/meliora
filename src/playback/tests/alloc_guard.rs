use crate::devices::dummy;
use crate::playback::thread::audio_engine::{AudioEngine, EngineCycleResult};
use crate::settings::equalizer::{EqBandKind, EqBandSettings, EqualizerSettings};
use crate::test_support::{TestDir, alloc_guard::count_allocations};

use super::harness::{
    configure_dummy_device, engine_lock, engine_playing, i16_test_signal, write_wav_i16,
};

const RATE: u32 = 44_100;
const FRAMES: usize = RATE as usize * 30;

#[test]
fn process_cycle_does_not_allocate() {
    let _guard = engine_lock();
    configure_dummy_device(RATE, "S16", 2);
    // 先卸载采集 sink，否则它自身的分配会混进被守护的循环里。
    dummy::uninstall_capture();

    let dir = TestDir::new("hb-alloc-guard");
    let path = dir.join("steady.wav");
    write_wav_i16(&path, RATE, 2, &i16_test_signal(FRAMES, 2));

    let mut engine = engine_playing(&path);
    assert_steady_state_cycles_allocate_nothing(&mut engine);
}

/// 激活态 EQ 的同款守卫：热环滤波体（Biquad 链 + 参数平滑 + 交叉淡化 + denormal
/// 清扫 + 补偿增益插值）的零分配不变量此前只对旁路成立——R26 深审发现守卫从未
/// 带可听 EQ 配置运行，滤波体里未来任何 `collect()`/`format!` 都不会被现有测试
/// 发现（10-02 的「audio callback 零分配」实测只覆盖了旁路形态）。
#[test]
fn process_cycle_with_active_equalizer_does_not_allocate() {
    let _guard = engine_lock();
    configure_dummy_device(RATE, "S16", 2);
    dummy::uninstall_capture();

    let dir = TestDir::new("hb-alloc-guard-eq");
    let path = dir.join("steady.wav");
    write_wav_i16(&path, RATE, 2, &i16_test_signal(FRAMES, 2));

    let mut engine = engine_playing(&path);
    // 可听配置：bell +12dB 让滤波主体全段工作，volume_compensation 让补偿增益
    // 的逐样本插值路径（eq.process 内）也在环上。
    let eq = EqualizerSettings {
        enabled: true,
        volume_compensation: true,
        bands: vec![EqBandSettings {
            kind: EqBandKind::Bell,
            frequency: 1_000.0,
            gain_db: 12.0,
            q: 1.0,
            enabled: true,
        }],
    };
    engine.set_equalizer(&eq);
    assert_steady_state_cycles_allocate_nothing(&mut engine);
}

/// 守护 `engine` 的全部稳态周期：逐周期计数分配，任一周期的分配都是失败。
/// set_equalizer 自身的分配发生在守护区之外（播放线程非 RT，10-02 已定案接受）。
fn assert_steady_state_cycles_allocate_nothing(engine: &mut AudioEngine) {
    let mut guarded_cycles = 0usize;
    let mut violating_cycles = 0usize;
    let mut total_allocations = 0u64;
    let mut first_violation = None;
    loop {
        let (result, allocations) = count_allocations(|| engine.process_cycle());
        match result {
            EngineCycleResult::Continue => {}
            EngineCycleResult::Eof => break,
            other => panic!("unexpected engine result under allocation guard: {other:?}"),
        }

        if allocations > 0 {
            violating_cycles += 1;
            total_allocations += allocations;
            first_violation.get_or_insert(guarded_cycles);
        }
        guarded_cycles += 1;
        assert!(
            guarded_cycles < 500_000,
            "engine never reached EOF under the allocation guard"
        );
    }

    assert!(
        guarded_cycles > 50,
        "only {guarded_cycles} guarded cycles ran; the guarded region is too \
         short to be meaningful"
    );
    assert_eq!(
        violating_cycles,
        0,
        "{violating_cycles} of {guarded_cycles} steady-state cycles allocated \
         ({total_allocations} allocations total, first at guarded cycle {})",
        first_violation.unwrap_or(0)
    );
}
