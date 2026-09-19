use crate::{
    devices::{
        errors::{
            CloseError, FindError, InfoError, OpenError, ResetError, StateError, SubmissionError,
        },
        format::{BufferSize, ChannelSpec, FormatInfo, SampleFormat},
        resample::SampleFrom,
        traits::{Device, DeviceProvider, OutputStream},
        util::{
            AtomicF64, DitherLsb, GainRamp, Scale, read_available, soft_limit, tpdf,
            write_bounded_planar,
        },
    },
    media::pipeline::{ChannelConsumers, DEFAULT_BUFFER_FRAMES},
};
use cpal::{
    Host, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rtrb::{Producer, RingBuffer};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

macro_rules! make_unknown_error {
    ($from:ty, $to:ty) => {
        impl From<$from> for $to {
            fn from(value: $from) -> Self {
                <$to>::Unknown(value.to_string())
            }
        }
    };
}

/// Delay between requesting a fade-out and pausing the stream. Must exceed
/// the gain ramp length (15 ms) plus a few callback periods to ensure the
/// audio thread drains a buffer at zero gain.
const PAUSE_FADE_WAIT: Duration = Duration::from_millis(50);
/// Target callback buffer size.
const DEVICE_BUFFER_TARGET: Duration = Duration::from_millis(20);
/// Internal ring buffer target. This stays larger than the device buffer so the
/// decoder can absorb scheduling jitter without underrunning the stream. 100 ms
/// underran whenever the producer stalled past one buffer (track opens,
/// online-stream fetches, cover-decode bursts); 250 ms absorbs those while
/// staying cheap (~1 MB) — seek/pause paths rebuild the stream via `reset()`,
/// so stale audio is never played from the slack.
const RING_BUFFER_TARGET: Duration = Duration::from_millis(250);
/// A producer stall at least this long is logged. The device ring is 250 ms, so
/// a stall near that length is what starves the realtime callback; measuring it
/// beats guessing at the cause of a bare `underran` line.
const PRODUCER_STALL_REPORT_MS: u64 = 100;

pub struct CpalProvider {
    host: Host,
}

impl Default for CpalProvider {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

impl DeviceProvider for CpalProvider {
    fn initialize(&mut self) {
        self.host = cpal::default_host();

        info!("Using cpal host {}", self.host.id().name());
    }

    fn get_default_device(&mut self) -> Result<Box<dyn Device>, FindError> {
        self.host
            .default_output_device()
            .ok_or(FindError::DeviceDoesNotExist)
            .map(|dev| Box::new(CpalDevice::from(dev)) as Box<dyn Device>)
    }
}

struct CpalDevice {
    device: cpal::Device,
}

impl From<cpal::Device> for CpalDevice {
    fn from(value: cpal::Device) -> Self {
        CpalDevice { device: value }
    }
}

impl TryFrom<cpal::SampleFormat> for SampleFormat {
    type Error = InfoError;

    fn try_from(value: cpal::SampleFormat) -> Result<Self, Self::Error> {
        match value {
            cpal::SampleFormat::I8 => Ok(SampleFormat::Signed8),
            cpal::SampleFormat::I16 => Ok(SampleFormat::Signed16),
            cpal::SampleFormat::I32 => Ok(SampleFormat::Signed32),
            cpal::SampleFormat::U8 => Ok(SampleFormat::Unsigned8),
            cpal::SampleFormat::U16 => Ok(SampleFormat::Unsigned16),
            cpal::SampleFormat::U32 => Ok(SampleFormat::Unsigned32),
            cpal::SampleFormat::F32 => Ok(SampleFormat::Float32),
            cpal::SampleFormat::F64 => Ok(SampleFormat::Float64),
            unsupported => Err(InfoError::SampleFmt(unsupported.to_string())),
        }
    }
}

fn cpal_config_from_info(format: &FormatInfo) -> Result<cpal::StreamConfig, ()> {
    if format.originating_provider != "cpal" {
        Err(())
    } else {
        let target_frames = frames_for_duration(format.sample_rate, DEVICE_BUFFER_TARGET);
        let buffer_size = match format.buffer_size {
            BufferSize::Range(min, max) => cpal::BufferSize::Fixed(target_frames.clamp(min, max)),
            BufferSize::Fixed(size) => cpal::BufferSize::Fixed(size),
            BufferSize::Unknown => cpal::BufferSize::Default,
        };

        Ok(cpal::StreamConfig {
            channels: format.channels.count(),
            sample_rate: format.sample_rate,
            buffer_size,
        })
    }
}

fn frames_for_duration(sample_rate: u32, duration: Duration) -> u32 {
    ((u64::from(sample_rate) * duration.as_micros() as u64) / 1_000_000).max(1) as u32
}

trait CpalSample: SizedSample + Default + Send + Sized + 'static + Scale {}

impl<T> CpalSample for T where T: SizedSample + Default + Send + Sized + 'static + Scale {}

fn create_stream_internal<T: CpalSample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    buffer_size: usize,
    target_gain: Arc<AtomicF64>,
    underruns: Arc<AtomicU64>,
    primed: Arc<AtomicBool>,
    device_error_message: Arc<Mutex<Option<String>>>,
) -> Result<(cpal::Stream, Producer<T>, Arc<AtomicBool>), OpenError> {
    let (prod, mut cons) = RingBuffer::<T>::new(buffer_size);
    let channels = config.channels as usize;
    let mut ramp = GainRamp::new(config.sample_rate);

    let device_errored = Arc::new(AtomicBool::new(false));
    let error_flag = device_errored.clone();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let read = read_available(&mut cons, data);
            if read < data.len() {
                // Zero the tail so an underrun plays silence instead of
                // whatever stale samples the device buffer still holds.
                data[read..].fill(T::default());
                // Before the producer's first submit the silence is the
                // expected open->prime gap (device stream runs while the
                // pipeline is still preparing the first track), not an
                // underrun — counting it logged a ~20-callback false burst
                // at every session start (2026-09 logs). Gate on primed so
                // the counter only reports real starvation of a fed stream.
                if primed.load(Ordering::Relaxed) {
                    underruns.fetch_add(1, Ordering::Relaxed);
                }
            }

            let target = target_gain.load(Ordering::Relaxed);
            ramp.apply(data, channels, target);
        },
        move |err| {
            // Realtime thread: record the message only. Logging (file IO)
            // happens on the producer side when it observes the flag.
            //
            // `try_lock`, not `lock`: the producer drains this same mutex, and
            // a realtime thread must never block on it. If the producer is
            // draining concurrently this message's text is dropped — during an
            // error storm dropping messages is acceptable, and the flag below
            // still trips the producer-side error path either way.
            if let Ok(mut slot) = device_error_message.try_lock() {
                *slot = Some(err.to_string());
            }
            error_flag.store(true, Ordering::Relaxed);
        },
        None,
    )?;

    Ok((stream, prod, device_errored))
}

impl CpalDevice {
    fn create_stream<T>(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError>
    where
        T: CpalSample + SampleFrom<f64> + DitherLsb,
    {
        let config =
            cpal_config_from_info(&format).map_err(|_| OpenError::InvalidConfigProvider)?;
        let channels = format.channels.count();
        let ring_buffer_frames = frames_for_duration(config.sample_rate, RING_BUFFER_TARGET);
        let buffer_size = ring_buffer_frames as usize * channels as usize;
        debug!(
            "CPAL buffer size {buffer_size}, \
            ring buffer {ring_buffer_frames} frames ({buffer_size} samples)",
        );
        info!(
            "audio ring: {ring_buffer_frames} frames ({} ms) × {channels} ch; device buffer target {} ms",
            RING_BUFFER_TARGET.as_millis(),
            DEVICE_BUFFER_TARGET.as_millis()
        );
        let target_gain = Arc::new(AtomicF64::new(1.0));
        let underruns = Arc::new(AtomicU64::new(0));
        let primed = Arc::new(AtomicBool::new(false));
        let device_error_message = Arc::new(Mutex::new(None::<String>));
        let (stream, prod, device_errored) = create_stream_internal::<T>(
            &self.device,
            config,
            buffer_size,
            target_gain.clone(),
            underruns.clone(),
            primed.clone(),
            device_error_message.clone(),
        )?;

        Ok(Box::new(CpalStream {
            ring_buf: prod,
            stream,
            config,
            buffer_size,
            device: self.device.clone(),
            target_gain,
            last_user_volume: 1.0,
            replaygain: 1.0,
            dither: 0x9E37_79B9_7F4A_7C15,
            // worst case: a full pipeline staging buffer, interleaved
            interleave_buffer: Vec::with_capacity(DEFAULT_BUFFER_FRAMES * channels as usize),
            underruns,
            primed,
            underruns_reported: 0,
            last_underrun_log: Instant::now(),
            stream_started_at: Instant::now(),
            idle_since: None,
            logged_first_submit: false,
            device_errored,
            device_error_message,
            pause_at: None,
        }))
    }
}

impl Device for CpalDevice {
    fn open_device(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError> {
        if format.originating_provider != "cpal" {
            Err(OpenError::InvalidConfigProvider)
        } else {
            match format.sample_type {
                SampleFormat::Signed8 => self.create_stream::<i8>(format),
                SampleFormat::Signed16 => self.create_stream::<i16>(format),
                SampleFormat::Signed32 => self.create_stream::<i32>(format),
                SampleFormat::Unsigned8 => self.create_stream::<u8>(format),
                SampleFormat::Unsigned16 => self.create_stream::<u16>(format),
                SampleFormat::Unsigned32 => self.create_stream::<u32>(format),
                SampleFormat::Float32 => self.create_stream::<f32>(format),
                SampleFormat::Float64 => self.create_stream::<f64>(format),
                _ => Err(OpenError::InvalidSampleFormat),
            }
        }
    }

    fn get_default_format(&self) -> Result<FormatInfo, InfoError> {
        let format = self.device.default_output_config()?;
        Ok(FormatInfo {
            originating_provider: "cpal",
            sample_type: format.sample_format().try_into()?,
            sample_rate: format.sample_rate(),
            buffer_size: match format.buffer_size() {
                &cpal::SupportedBufferSize::Range { min, max } => BufferSize::Range(min, max),
                cpal::SupportedBufferSize::Unknown => BufferSize::Unknown,
            },
            channels: ChannelSpec::Count(format.channels()),
        })
    }

    fn get_name(&self) -> Result<String, InfoError> {
        self.device
            .description()
            .map_err(|v| v.into())
            .map(|v| v.name().to_string())
    }
}

struct CpalStream<T>
where
    T: SizedSample + Default,
{
    pub ring_buf: Producer<T>,
    pub stream: cpal::Stream,
    pub config: cpal::StreamConfig,
    pub device: cpal::Device,
    pub buffer_size: usize,
    pub target_gain: Arc<AtomicF64>,
    /// most recent volume the user asked for. This is tracked separately
    /// from `target_gain` because pause-fades temporarily overwrite the
    /// shared atomic with 0.0. `play()` restores from this field.
    pub last_user_volume: f64,
    pub replaygain: f64,
    /// xorshift state feeding per-sample TPDF dither (nonzero).
    pub dither: u64,
    pub interleave_buffer: Vec<T>,
    pub underruns: Arc<AtomicU64>,
    /// Cleared until the producer's first successful submit (and again on
    /// `reset`): the realtime callback counts underruns only while this is
    /// set, so the open->prime silence gap is not counted as starvation.
    pub primed: Arc<AtomicBool>,
    /// keep track of the last log, so we don't log the same underrun multiple times
    underruns_reported: u64,
    last_underrun_log: Instant,
    /// When this stream began consuming, used to time the open -> first-audio gap.
    stream_started_at: Instant,
    /// When the producer last had nothing to submit, cleared on the first
    /// successful submit. Producer-side only, so the realtime callback stays
    /// untouched.
    idle_since: Option<Instant>,
    logged_first_submit: bool,
    device_errored: Arc<AtomicBool>,
    /// Latest cpal error message, written by the realtime error callback and
    /// drained (logged) by the producer. Keeps logging off the audio thread.
    device_error_message: Arc<Mutex<Option<String>>>,
    /// Indicates that the stream is currently fading out and needs to be paused by the specified
    /// time.
    pause_at: Option<Instant>,
}

impl<T> CpalStream<T>
where
    T: SizedSample + Default,
{
    fn report_underruns(&mut self) {
        let total = self.underruns.load(Ordering::Relaxed);
        if total > self.underruns_reported
            && self.last_underrun_log.elapsed() >= Duration::from_secs(1)
        {
            warn!(
                "audio callback underran {} time(s) ({} total)",
                total - self.underruns_reported,
                total
            );
            self.underruns_reported = total;
            self.last_underrun_log = Instant::now();
        }
    }
}

impl<T> OutputStream for CpalStream<T>
where
    T: CpalSample + SampleFrom<f64> + DitherLsb,
{
    fn close_stream(&mut self) -> Result<(), CloseError> {
        Ok(())
    }

    fn play(&mut self) -> Result<(), StateError> {
        self.pause_at = None;
        self.target_gain
            .store(self.last_user_volume, Ordering::Relaxed);
        self.stream.play().map_err(|v| v.into())
    }

    fn pause(&mut self) -> Result<(), StateError> {
        self.target_gain.store(0.0, Ordering::Relaxed);
        self.pause_at = Some(Instant::now() + PAUSE_FADE_WAIT);
        Ok(())
    }

    fn poll(&mut self) -> Result<(), StateError> {
        if let Some(deadline) = self.pause_at
            && Instant::now() >= deadline
        {
            self.pause_at = None;
            return self.stream.pause().map_err(|v| v.into());
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), ResetError> {
        let pause_pending = self.pause_at.take().is_some();
        let (stream, prod, device_errored) = create_stream_internal::<T>(
            &self.device,
            self.config,
            self.buffer_size,
            self.target_gain.clone(),
            self.underruns.clone(),
            self.primed.clone(),
            self.device_error_message.clone(),
        )?;

        self.stream = stream;
        self.ring_buf = prod;
        self.device_errored = device_errored;
        self.interleave_buffer.clear();
        self.stream_started_at = Instant::now();
        self.idle_since = None;
        self.logged_first_submit = false;
        // The rebuilt stream is unprimed until samples flow again.
        self.primed.store(false, Ordering::Relaxed);

        if pause_pending && let Err(e) = self.stream.pause() {
            return Err(ResetError::Unknown(e.to_string()));
        }

        Ok(())
    }

    fn set_volume(&mut self, volume: f64) -> Result<(), StateError> {
        self.last_user_volume = volume;
        if self.pause_at.is_none() {
            self.target_gain.store(volume, Ordering::Relaxed);
        }
        Ok(())
    }

    fn set_replaygain(&mut self, gain: f64) -> Result<(), StateError> {
        self.replaygain = gain;
        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    fn consume_from(&mut self, input: &mut ChannelConsumers) -> Result<usize, SubmissionError> {
        if self.device_errored.load(Ordering::Relaxed) {
            // Take the message under the lock, log outside of it: the realtime
            // error callback shares this mutex and must never be kept waiting
            // behind file IO. Recover from a poisoned lock so a panic in some
            // other lock user can never make the message unrecoverable.
            let message = self
                .device_error_message
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(msg) = message {
                warn!("cpal stream error: {msg}");
            }
            return Err(SubmissionError::DeviceError);
        }

        self.report_underruns();

        let capacity_frames = self.interleave_buffer.capacity() / input.channel_count().max(1);
        let available = input.potentially_available().min(capacity_frames);
        if available == 0 {
            self.idle_since.get_or_insert_with(Instant::now);
            return Ok(0);
        }

        let read = input.try_read_to_staging(available);
        if read == 0 {
            self.idle_since.get_or_insert_with(Instant::now);
            return Ok(0);
        }

        // How long the producer had nothing to hand the device ring is what
        // actually starves the realtime callback, and it is the one number a
        // bare `underran` line does not give. Logged on the idle -> fed
        // transition only, so steady playback stays quiet in the log.
        if let Some(since) = self.idle_since.take() {
            let idle_ms = since.elapsed().as_millis() as u64;
            if idle_ms >= PRODUCER_STALL_REPORT_MS {
                info!(
                    idle_ms,
                    underruns = self.underruns.load(Ordering::Relaxed),
                    "[audio] producer had nothing to submit"
                );
            }
        }
        if !self.logged_first_submit {
            self.logged_first_submit = true;
            info!(
                elapsed_ms = self.stream_started_at.elapsed().as_millis() as u64,
                "[audio] first samples submitted after stream start"
            );
        }

        let staging = input.staging();

        let channel_count = staging.len();
        let rg = self.replaygain;
        let mut dither = self.dither;

        self.interleave_buffer.clear();
        debug_assert!(
            read * channel_count <= self.interleave_buffer.capacity(),
            "interleave buffer under-sized at stream creation"
        );

        for i in 0..read {
            for ch in 0..channel_count {
                // Soft limiting before quantization: gain (ReplayGain / EQ
                // boost) curves into full scale instead of hard clipping.
                let sample = soft_limit(staging[ch][i] * rg);
                // TPDF dither decorrelates quantization error for shallow
                // integer targets; floats and ≥24-bit convert untouched.
                let sample = match T::DITHER_LSB {
                    Some(lsb) => sample + tpdf(&mut dither) * lsb,
                    None => sample,
                };
                self.interleave_buffer.push(T::sample_from(sample));
            }
        }
        self.dither = dither;

        write_bounded_planar(
            std::slice::from_mut(&mut self.ring_buf),
            &[&self.interleave_buffer],
            self.interleave_buffer.len(),
        )
        .map_err(|_| SubmissionError::WriteTimeout)?;
        // Samples are in the ring: from here on a starving callback is a
        // real underrun and gets counted.
        self.primed.store(true, Ordering::Relaxed);

        Ok(read)
    }
}

make_unknown_error!(OpenError, ResetError);
make_unknown_error!(cpal::Error, StateError);
make_unknown_error!(cpal::Error, InfoError);
make_unknown_error!(cpal::Error, OpenError);
make_unknown_error!(cpal::Error, FindError);
