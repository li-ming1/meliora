pub mod lofty;
pub mod symphonia;

/// Extensions every builtin provider claims; both parse the same formats.
pub const SUPPORTED_EXTENSIONS: &[&str] =
    &["ogg", "oga", "aac", "flac", "wav", "mp3", "m4a", "aiff", "opus"];
