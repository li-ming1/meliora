
use crate::media::pipeline::ChannelConsumers;

use super::{
    errors::{
        CloseError, FindError, InfoError, OpenError, ResetError, StateError, SubmissionError,
    },
    format::FormatInfo,
};

/// The DeviceProvider trait defines the methods used to interact with a device provider. A device
/// provider is responsible for providing a list of devices available to the system, as well as
/// opening and closing streams on those devices.
///
/// The current audio pipeline is as follows:
pub trait DeviceProvider {
    /// Requests the device provider prepare itself for use.
    fn initialize(&mut self);
    /// Returns the default device of the device provider.
    fn get_default_device(&mut self) -> Result<Box<dyn Device>, FindError>;
}

pub trait Device {
    /// Requests the device open a stream with the given format.
    fn open_device(&mut self, format: FormatInfo) -> Result<Box<dyn OutputStream>, OpenError>;

    /// Returns the device's default format.
    fn get_default_format(&self) -> Result<FormatInfo, InfoError>;
    /// Returns the name of the device.
    fn get_name(&self) -> Result<String, InfoError>;
}

pub trait OutputStream {
    /// Closes the stream and releases any resources associated with it.
    fn close_stream(&mut self) -> Result<(), CloseError>;
    /// Tells the device to start playing audio.
    fn play(&mut self) -> Result<(), StateError>;
    /// Tells the device to stop playing audio. Note that some providers may not actually stop
    /// playback at all - this function may be a no-op. Submitting frames after calling this
    /// without calling play is undefined behavior, and may result in the thread blocking
    /// indefinitely.
    ///
    /// When implementing this function, the device should never drop submitted audio data. If the
    /// options are between dropping audio data and this function being a no-op, the function
    /// should be a no-op.
    fn pause(&mut self) -> Result<(), StateError>;
    /// Advances deferred stream work; called every iteration of the playback thread's main loop.
    /// Lets `pause()` return immediately and finish its fade-out here later. Default: no-op.
    fn poll(&mut self) -> Result<(), StateError> {
        Ok(())
    }
    /// Tells the device to reset the buffer. This is useful for restarting playback after a pause,
    /// in order to avoid playing stale data (e.g. if a user pauses before seeking or changing
    /// tracks).
    fn reset(&mut self) -> Result<(), ResetError>;
    /// Tells the device to set the volume to the given value. The volume should be a value between
    /// 0.0 and 1.0. Note that some device providers may not support hardware or OS-level volume
    /// control, and will instead use this value to adjust the volume of the audio data before
    /// submitting it to the device.
    fn set_volume(&mut self, volume: f64) -> Result<(), StateError>;

    /// Sets the ReplayGain multiplier. Applied on top of volume.
    fn set_replaygain(&mut self, _gain: f64) -> Result<(), StateError> {
        Ok(())
    }

    /// Consume samples from ring buffer consumers and submit them to the device.
    fn consume_from(&mut self, input: &mut ChannelConsumers)
    -> Result<usize, SubmissionError>;
}
