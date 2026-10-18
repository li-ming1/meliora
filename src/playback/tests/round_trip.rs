use intx::{I24, U24};

use crate::devices::resample::{SampleFrom, SampleInto};

use super::harness::xorshift64;

const DENSE_SWEEP_SAMPLES: usize = 1_000_000;

fn round_trips<T>(value: T) -> bool
where
    T: SampleInto<f64> + SampleFrom<f64> + PartialEq + Copy,
{
    T::sample_from(value.sample_into()) == value
}

#[test]
fn i8_round_trip_exhaustive() {
    let failures = (i8::MIN..=i8::MAX).filter(|&v| !round_trips(v)).count();
    assert_eq!(
        failures, 0,
        "{failures} of 256 i8 values fail to round-trip"
    );
}

#[test]
fn u8_round_trip_exhaustive() {
    let failures = (u8::MIN..=u8::MAX).filter(|&v| !round_trips(v)).count();
    assert_eq!(
        failures, 0,
        "{failures} of 256 u8 values fail to round-trip"
    );
}

#[test]
fn i16_round_trip_exhaustive() {
    let failures = (i16::MIN..=i16::MAX).filter(|&v| !round_trips(v)).count();
    assert_eq!(
        failures, 0,
        "{failures} of 65536 i16 values fail to round-trip"
    );
}

#[test]
fn u16_round_trip_exhaustive() {
    let failures = (u16::MIN..=u16::MAX).filter(|&v| !round_trips(v)).count();
    assert_eq!(
        failures, 0,
        "{failures} of 65536 u16 values fail to round-trip"
    );
}

// 理论上能把 2^24 个值全穷举一遍，但 16 位之后取值空间太大，
// 密采样几乎不会漏掉真问题。
#[test]
fn i24_round_trip_dense() {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let mut failures = 0;
    for _ in 0..DENSE_SWEEP_SAMPLES {
        let raw = (xorshift64(&mut state) & 0xFF_FFFF) as i32 - 0x80_0000;
        if !round_trips(I24::try_from(raw).unwrap()) {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of {DENSE_SWEEP_SAMPLES} I24 values fail to round-trip"
    );
}

#[test]
fn u24_round_trip_dense() {
    let mut state = 0xC2B2_AE3D_27D4_EB4F;
    let mut failures = 0;
    for _ in 0..DENSE_SWEEP_SAMPLES {
        let raw = (xorshift64(&mut state) & 0xFF_FFFF) as u32;
        if !round_trips(U24::try_from(raw).unwrap()) {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of {DENSE_SWEEP_SAMPLES} U24 values fail to round-trip"
    );
}

#[test]
fn i32_round_trip_dense() {
    let mut state = 0x165E_67B1_35BF_89AB;
    let mut failures = 0;
    for _ in 0..DENSE_SWEEP_SAMPLES {
        if !round_trips(xorshift64(&mut state) as u32 as i32) {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of {DENSE_SWEEP_SAMPLES} i32 values fail to round-trip"
    );
}

#[test]
fn u32_round_trip_dense() {
    let mut state = 0xDA94_2042_E4DD_58B5;
    let mut failures = 0;
    for _ in 0..DENSE_SWEEP_SAMPLES {
        if !round_trips(xorshift64(&mut state) as u32) {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of {DENSE_SWEEP_SAMPLES} u32 values fail to round-trip"
    );
}

#[test]
fn f32_round_trip_dense() {
    let mut state = 0x0308_1E85_20C2_2FBD;
    let mut failures = 0;
    for _ in 0..DENSE_SWEEP_SAMPLES {
        let v = (xorshift64(&mut state) as u32 as i32) as f32 / (i32::MAX as f32 + 1.0);
        // 按比特比较，比 `round_trips` 内部的 PartialEq 更严：
        // -0.0 与 0.0 相等但比特形态不同
        if f32::sample_from(SampleInto::<f64>::sample_into(v)).to_bits() != v.to_bits() {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of {DENSE_SWEEP_SAMPLES} f32 values fail to round-trip through f64"
    );
}

#[test]
fn edge_values_round_trip() {
    // 有符号极值与零
    assert!(round_trips(i8::MIN));
    assert!(round_trips(i8::MAX));
    assert!(round_trips(0i8));
    assert!(round_trips(i16::MIN));
    assert!(round_trips(i16::MAX));
    assert!(round_trips(0i16));
    assert!(round_trips(i32::MIN));
    assert!(round_trips(i32::MAX));
    assert!(round_trips(0i32));
    assert!(round_trips(I24::MIN));
    assert!(round_trips(I24::MAX));
    assert!(round_trips(I24::try_from(0i32).unwrap()));

    // 无符号极值与中点（数字静音）
    assert!(round_trips(u8::MIN));
    assert!(round_trips(u8::MAX));
    assert!(round_trips(1u8 << 7));
    assert!(round_trips(u16::MIN));
    assert!(round_trips(u16::MAX));
    assert!(round_trips(1u16 << 15));
    assert!(round_trips(u32::MIN));
    assert!(round_trips(u32::MAX));
    assert!(round_trips(1u32 << 31));
    assert!(round_trips(U24::try_from(0u32).unwrap()));
    assert!(round_trips(U24::try_from(0xFF_FFFFu32).unwrap()));
    assert!(round_trips(U24::try_from(1u32 << 23).unwrap()));

    // 浮点特殊值
    assert!(round_trips(0.0f32));
    assert!(round_trips(1.0f32));
    assert!(round_trips(-1.0f32));
    assert!(round_trips(0.0f64));
    assert!(round_trips(1.0f64));
    assert!(round_trips(-1.0f64));
}

#[test]
fn unsigned_midpoint_is_digital_silence() {
    let mid_u8: f64 = (1u8 << 7).sample_into();
    assert_eq!(mid_u8, 0.0, "u8 midpoint maps to {mid_u8}, not 0.0");

    let mid_u16: f64 = (1u16 << 15).sample_into();
    assert_eq!(mid_u16, 0.0, "u16 midpoint maps to {mid_u16}, not 0.0");

    let mid_u24: f64 = U24::try_from(1u32 << 23).unwrap().sample_into();
    assert_eq!(mid_u24, 0.0, "U24 midpoint maps to {mid_u24}, not 0.0");

    let mid_u32: f64 = (1u32 << 31).sample_into();
    assert_eq!(mid_u32, 0.0, "u32 midpoint maps to {mid_u32}, not 0.0");
}

#[test]
fn scaled_conversion_is_total() {
    // 略微超出 [-1.0, 1.0] 的值专门压饱和转换（旧的 /32767 缩放下
    // i16::MIN 算出来的正是这类值）
    let bases = [-1.0000305, -1.0, -0.5, 0.0, 0.5, 1.0, 1.0000305];
    for gain in [0.5, 1.0, 2.0] {
        for base in bases {
            let v: f64 = base * gain;
            let _ = i8::sample_from(v);
            let _ = u8::sample_from(v);
            let _ = i16::sample_from(v);
            let _ = u16::sample_from(v);
            let _ = i32::sample_from(v);
            let _ = u32::sample_from(v);
            let _ = I24::sample_from(v);
            let _ = U24::sample_from(v);
            let _ = f32::sample_from(v);
        }
    }
}

#[test]
fn float_to_int_rounds_to_nearest() {
    // 100.4 / 32768 应落在 100、100.6 应落在 101 —— 截断会让两者都得 100
    assert_eq!(i16::sample_from(100.4 / 32_768.0f64), 100);
    assert_eq!(i16::sample_from(100.6 / 32_768.0f64), 101);
    assert_eq!(i16::sample_from(-100.4 / 32_768.0f64), -100);
    assert_eq!(i16::sample_from(-100.6 / 32_768.0f64), -101);
    assert_eq!(i8::sample_from(63.7 / 128.0f64), 64);
    assert_eq!(
        I24::sample_from(1000.6 / 8_388_608.0f64),
        I24::try_from(1001).unwrap()
    );
}

#[test]
fn out_of_range_floats_saturate() {
    assert_eq!(i16::sample_from(2.0f64), i16::MAX);
    assert_eq!(i16::sample_from(-2.0f64), i16::MIN);
    assert_eq!(u16::sample_from(2.0f64), u16::MAX);
    assert_eq!(u16::sample_from(-2.0f64), u16::MIN);
    assert_eq!(i32::sample_from(2.0f64), i32::MAX);
    assert_eq!(I24::sample_from(2.0f64), I24::MAX);
    assert_eq!(I24::sample_from(-2.0f64), I24::MIN);
    assert_eq!(
        U24::sample_from(2.0f64),
        U24::try_from(0xFF_FFFFu32).unwrap()
    );
    assert_eq!(U24::sample_from(-2.0f64), U24::try_from(0u32).unwrap());
    // NaN 不得 panic；它落到饱和转换给出的值（0 -> 中点）
    assert_eq!(i16::sample_from(f64::NAN), 0);
    assert_eq!(u16::sample_from(f64::NAN), 1u16 << 15);
}

#[test]
fn signed_extremes_map_symmetrically() {
    // ÷2^(N-1) 缩放：MIN 精确落到 -1.0，静音精确落到 0.0
    assert_eq!(SampleInto::<f64>::sample_into(i16::MIN), -1.0);
    assert_eq!(SampleInto::<f64>::sample_into(0i16), 0.0);
    assert_eq!(SampleInto::<f64>::sample_into(i32::MIN), -1.0);
    assert_eq!(SampleInto::<f64>::sample_into(I24::MIN), -1.0);
    assert_eq!(SampleInto::<f64>::sample_into(u16::MIN), -1.0);
    assert_eq!(
        SampleInto::<f64>::sample_into(u16::MAX),
        32_767.0 / 32_768.0
    );
}

#[test]
fn full_scale_floats_reach_integer_extremes() {
    assert_eq!(i16::sample_from(1.0f64), i16::MAX);
    assert_eq!(i16::sample_from(0.0f64), 0);
    assert_eq!(i32::sample_from(1.0f64), i32::MAX);
    assert_eq!(I24::sample_from(1.0f64), I24::MAX);
    assert_eq!(u16::sample_from(1.0f64), u16::MAX);
    assert_eq!(u8::sample_from(1.0f64), u8::MAX);
}
