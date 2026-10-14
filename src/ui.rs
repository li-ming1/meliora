mod about;
pub mod app;
mod artist_picker;
mod assets;
pub mod availability;
pub(crate) mod caching;
mod command_palette;
pub mod components;
mod constants;
mod controls;
pub(crate) mod design;
pub mod desktop_lyrics;
mod equalizer;
mod global_actions;
mod header;
mod immersive;
mod keymap;
#[cfg(feature = "kugou")]
mod kugou;
pub mod library;
pub mod library_access;
mod lyrics;
pub mod models;
#[cfg(feature = "kugou")]
mod online_common;
#[cfg(feature = "kugou")]
mod online_track_row;
mod playback_bridge;
mod queue;
pub mod queue_metadata;
mod right_sidebar;
mod scroll_follow;
mod search;
mod settings;
mod theme;
mod toasts;
mod troubleshooting;
pub mod util;
