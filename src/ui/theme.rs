use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, mpsc::channel},
    time::Duration,
};

use crate::settings::SettingsGlobal;
use gpui::{App, AppContext, AsyncApp, Entity, EventEmitter, Global, Rgba, rgb, rgba};
use notify::{Event, RecursiveMode, Watcher};
use serde::{
    Deserialize, Deserializer,
    de::{Error as SerdeError, IgnoredAny, MapAccess, Visitor},
};
use tracing::{error, info, warn};

/// A color parsed from the CSS-style hex strings used in theme files.
struct ColorHex(Rgba);

impl<'de> Deserialize<'de> for ColorHex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ColorHexVisitor;

        impl<'de> Visitor<'de> for ColorHexVisitor {
            type Value = ColorHex;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string in the format #rgb, #rgba, #rrggbb, or #rrggbbaa")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: SerdeError,
            {
                parse_hex_color(value).map(ColorHex).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ColorHexVisitor)
    }
}

fn parse_hex_color(value: &str) -> Result<Rgba, String> {
    const EXPECTED: &str = "expected #rgb, #rgba, #rrggbb, or #rrggbbaa";

    let Some(hex) = value.trim().strip_prefix('#') else {
        return Err(format!("invalid hex color '{value}': {EXPECTED}"));
    };

    fn component(hex: &str, range: std::ops::Range<usize>, duplicate: bool) -> Result<f32, String> {
        let digits = hex
            .get(range)
            .ok_or_else(|| format!("invalid hex color '{hex}'"))?;
        let v = u8::from_str_radix(digits, 16).map_err(|_| format!("invalid hex color '{hex}'"))?;
        Ok(if duplicate { (v << 4) | v } else { v } as f32 / 255.0)
    }

    let (r, g, b, a) = match hex.len() {
        3 | 4 => (
            component(hex, 0..1, true)?,
            component(hex, 1..2, true)?,
            component(hex, 2..3, true)?,
            component(hex, 3..4, true).unwrap_or(1.0),
        ),
        6 | 8 => (
            component(hex, 0..2, false)?,
            component(hex, 2..4, false)?,
            component(hex, 4..6, false)?,
            component(hex, 6..8, false).unwrap_or(1.0),
        ),
        _ => return Err(format!("invalid hex color '{value}': {EXPECTED}")),
    };

    Ok(Rgba::new(r, g, b, a))
}

#[derive(Clone)]
pub struct Theme {
    pub background_primary: Rgba,
    pub background_secondary: Rgba,
    pub background_tertiary: Rgba,

    pub border_color: Rgba,

    /// Corner radius scale in px, consumed by components instead of hardcoded px() values.
    pub radius_sm: f32,
    pub radius_md: f32,
    pub radius_lg: f32,

    pub album_art_background: Rgba,

    pub text: Rgba,
    pub text_secondary: Rgba,
    pub text_disabled: Rgba,
    pub text_link: Rgba,

    pub nav_button_hover: Rgba,
    pub nav_button_hover_border: Rgba,
    pub nav_button_active: Rgba,
    pub nav_button_active_border: Rgba,
    pub nav_button_pressed: Rgba,
    pub nav_button_pressed_border: Rgba,

    pub playback_button: Rgba,
    pub playback_button_hover: Rgba,
    pub playback_button_active: Rgba,
    pub playback_button_border: Rgba,
    pub playback_button_toggled: Rgba,
    pub playback_button_repeat_one: Rgba,
    pub stop_after_current_indicator: Rgba,

    pub window_button: Rgba,
    pub window_button_hover: Rgba,
    pub window_button_active: Rgba,

    pub close_button: Rgba,
    pub close_button_hover: Rgba,
    pub close_button_active: Rgba,

    pub queue_item: Rgba,
    pub queue_item_hover: Rgba,
    pub queue_item_active: Rgba,
    pub queue_item_current: Rgba,
    pub queue_item_selected: Rgba,

    pub button_primary: Rgba,
    pub button_primary_border: Rgba,
    pub button_primary_hover: Rgba,
    pub button_primary_border_hover: Rgba,
    pub button_primary_active: Rgba,
    pub button_primary_border_active: Rgba,
    pub button_primary_text: Rgba,

    pub button_secondary: Rgba,
    pub button_secondary_border: Rgba,
    pub button_secondary_hover: Rgba,
    pub button_secondary_border_hover: Rgba,
    pub button_secondary_active: Rgba,
    pub button_secondary_border_active: Rgba,
    pub button_secondary_text: Rgba,

    pub button_warning: Rgba,
    pub button_warning_border: Rgba,
    pub button_warning_hover: Rgba,
    pub button_warning_border_hover: Rgba,
    pub button_warning_active: Rgba,
    pub button_warning_border_active: Rgba,
    pub button_warning_text: Rgba,

    pub button_danger: Rgba,
    pub button_danger_border: Rgba,
    pub button_danger_hover: Rgba,
    pub button_danger_border_hover: Rgba,
    pub button_danger_active: Rgba,
    pub button_danger_border_active: Rgba,
    pub button_danger_text: Rgba,

    pub slider_foreground: Rgba,
    pub slider_background: Rgba,

    pub eq_grid_line: Rgba,
    pub eq_grid_line_zero: Rgba,
    pub eq_curve: Rgba,
    pub eq_curve_fill: Rgba,
    pub eq_band_curve: Rgba,
    pub eq_dot: Rgba,
    pub eq_dot_selected: Rgba,
    pub eq_dot_disabled: Rgba,
    pub eq_spectrum_pre: Rgba,
    pub eq_spectrum_post: Rgba,
    pub eq_spectrum_edge: Rgba,

    pub elevated_background: Rgba,
    pub elevated_border_color: Rgba,

    pub menu_item: Rgba,
    pub menu_item_hover: Rgba,
    pub menu_item_border_hover: Rgba,
    pub menu_item_active: Rgba,
    pub menu_item_border_active: Rgba,

    pub modal_overlay_bg: Rgba,

    pub text_input_selection: Rgba,
    pub caret_color: Rgba,
    pub text_highlight_background: Rgba,

    pub palette_item_hover: Rgba,
    pub palette_item_border_hover: Rgba,
    pub palette_item_active: Rgba,
    pub palette_item_border_active: Rgba,

    pub scrollbar_background: Rgba,
    pub scrollbar_foreground: Rgba,

    pub textbox_background: Rgba,
    pub textbox_border: Rgba,

    pub checkbox_background: Rgba,
    pub checkbox_background_hover: Rgba,
    pub checkbox_background_active: Rgba,
    pub checkbox_border: Rgba,
    pub checkbox_border_hover: Rgba,
    pub checkbox_border_active: Rgba,
    pub checkbox_checked: Rgba,
    pub checkbox_checked_bg: Rgba,
    pub checkbox_checked_bg_hover: Rgba,
    pub checkbox_checked_bg_active: Rgba,
    pub checkbox_checked_border: Rgba,
    pub checkbox_checked_border_hover: Rgba,
    pub checkbox_checked_border_active: Rgba,

    pub callout_background: Rgba,
    pub callout_border: Rgba,
    pub callout_text: Rgba,

    pub liked_song: Rgba,

    pub status_success: Rgba,
    pub status_error: Rgba,
    pub status_disabled: Rgba,

    pub toast_info_background: Rgba,
    pub toast_info_border: Rgba,
    pub toast_info_text: Rgba,
    pub toast_info_track: Rgba,

    pub toast_warning_background: Rgba,
    pub toast_warning_border: Rgba,
    pub toast_warning_text: Rgba,
    pub toast_warning_track: Rgba,

    pub toast_success_background: Rgba,
    pub toast_success_border: Rgba,
    pub toast_success_text: Rgba,
    pub toast_success_track: Rgba,

    pub toast_error_background: Rgba,
    pub toast_error_border: Rgba,
    pub toast_error_text: Rgba,
    pub toast_error_track: Rgba,
}

impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ThemeVisitor;

        impl<'de> Visitor<'de> for ThemeVisitor {
            type Value = Theme;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of color names to hex strings")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut theme = Theme::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "background_primary" => {
                            theme.background_primary = map.next_value::<ColorHex>()?.0
                        }
                        "background_secondary" => {
                            theme.background_secondary = map.next_value::<ColorHex>()?.0
                        }
                        "background_tertiary" => {
                            theme.background_tertiary = map.next_value::<ColorHex>()?.0
                        }
                        "border_color" => theme.border_color = map.next_value::<ColorHex>()?.0,
                        "album_art_background" => {
                            theme.album_art_background = map.next_value::<ColorHex>()?.0
                        }
                        "text" => theme.text = map.next_value::<ColorHex>()?.0,
                        "text_secondary" => theme.text_secondary = map.next_value::<ColorHex>()?.0,
                        "text_disabled" => theme.text_disabled = map.next_value::<ColorHex>()?.0,
                        "text_link" => theme.text_link = map.next_value::<ColorHex>()?.0,
                        "nav_button_hover" => {
                            theme.nav_button_hover = map.next_value::<ColorHex>()?.0
                        }
                        "nav_button_hover_border" => {
                            theme.nav_button_hover_border = map.next_value::<ColorHex>()?.0
                        }
                        "nav_button_active" => {
                            theme.nav_button_active = map.next_value::<ColorHex>()?.0
                        }
                        "nav_button_active_border" => {
                            theme.nav_button_active_border = map.next_value::<ColorHex>()?.0
                        }
                        "nav_button_pressed" => {
                            theme.nav_button_pressed = map.next_value::<ColorHex>()?.0
                        }
                        "nav_button_pressed_border" => {
                            theme.nav_button_pressed_border = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button" => {
                            theme.playback_button = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button_hover" => {
                            theme.playback_button_hover = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button_active" => {
                            theme.playback_button_active = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button_border" => {
                            theme.playback_button_border = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button_toggled" => {
                            theme.playback_button_toggled = map.next_value::<ColorHex>()?.0
                        }
                        "playback_button_repeat_one" => {
                            theme.playback_button_repeat_one = map.next_value::<ColorHex>()?.0
                        }
                        "stop_after_current_indicator" => {
                            theme.stop_after_current_indicator = map.next_value::<ColorHex>()?.0
                        }
                        "window_button" => theme.window_button = map.next_value::<ColorHex>()?.0,
                        "window_button_hover" => {
                            theme.window_button_hover = map.next_value::<ColorHex>()?.0
                        }
                        "window_button_active" => {
                            theme.window_button_active = map.next_value::<ColorHex>()?.0
                        }
                        "close_button" => theme.close_button = map.next_value::<ColorHex>()?.0,
                        "close_button_hover" => {
                            theme.close_button_hover = map.next_value::<ColorHex>()?.0
                        }
                        "close_button_active" => {
                            theme.close_button_active = map.next_value::<ColorHex>()?.0
                        }
                        "queue_item" => theme.queue_item = map.next_value::<ColorHex>()?.0,
                        "queue_item_hover" => {
                            theme.queue_item_hover = map.next_value::<ColorHex>()?.0
                        }
                        "queue_item_active" => {
                            theme.queue_item_active = map.next_value::<ColorHex>()?.0
                        }
                        "queue_item_current" => {
                            theme.queue_item_current = map.next_value::<ColorHex>()?.0
                        }
                        "queue_item_selected" => {
                            theme.queue_item_selected = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary" => theme.button_primary = map.next_value::<ColorHex>()?.0,
                        "button_primary_border" => {
                            theme.button_primary_border = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary_hover" => {
                            theme.button_primary_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary_border_hover" => {
                            theme.button_primary_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary_active" => {
                            theme.button_primary_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary_border_active" => {
                            theme.button_primary_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_primary_text" => {
                            theme.button_primary_text = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary" => {
                            theme.button_secondary = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_border" => {
                            theme.button_secondary_border = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_hover" => {
                            theme.button_secondary_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_border_hover" => {
                            theme.button_secondary_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_active" => {
                            theme.button_secondary_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_border_active" => {
                            theme.button_secondary_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_secondary_text" => {
                            theme.button_secondary_text = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning" => theme.button_warning = map.next_value::<ColorHex>()?.0,
                        "button_warning_border" => {
                            theme.button_warning_border = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning_hover" => {
                            theme.button_warning_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning_border_hover" => {
                            theme.button_warning_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning_active" => {
                            theme.button_warning_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning_border_active" => {
                            theme.button_warning_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_warning_text" => {
                            theme.button_warning_text = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger" => theme.button_danger = map.next_value::<ColorHex>()?.0,
                        "button_danger_border" => {
                            theme.button_danger_border = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger_hover" => {
                            theme.button_danger_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger_border_hover" => {
                            theme.button_danger_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger_active" => {
                            theme.button_danger_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger_border_active" => {
                            theme.button_danger_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "button_danger_text" => {
                            theme.button_danger_text = map.next_value::<ColorHex>()?.0
                        }
                        "slider_foreground" => {
                            theme.slider_foreground = map.next_value::<ColorHex>()?.0
                        }
                        "slider_background" => {
                            theme.slider_background = map.next_value::<ColorHex>()?.0
                        }
                        "eq_grid_line" => theme.eq_grid_line = map.next_value::<ColorHex>()?.0,
                        "eq_grid_line_zero" => {
                            theme.eq_grid_line_zero = map.next_value::<ColorHex>()?.0
                        }
                        "eq_curve" => theme.eq_curve = map.next_value::<ColorHex>()?.0,
                        "eq_curve_fill" => theme.eq_curve_fill = map.next_value::<ColorHex>()?.0,
                        "eq_band_curve" => theme.eq_band_curve = map.next_value::<ColorHex>()?.0,
                        "eq_dot" => theme.eq_dot = map.next_value::<ColorHex>()?.0,
                        "eq_dot_selected" => {
                            theme.eq_dot_selected = map.next_value::<ColorHex>()?.0
                        }
                        "eq_dot_disabled" => {
                            theme.eq_dot_disabled = map.next_value::<ColorHex>()?.0
                        }
                        "eq_spectrum_pre" => {
                            theme.eq_spectrum_pre = map.next_value::<ColorHex>()?.0
                        }
                        "eq_spectrum_post" => {
                            theme.eq_spectrum_post = map.next_value::<ColorHex>()?.0
                        }
                        "eq_spectrum_edge" => {
                            theme.eq_spectrum_edge = map.next_value::<ColorHex>()?.0
                        }
                        "elevated_background" => {
                            theme.elevated_background = map.next_value::<ColorHex>()?.0
                        }
                        "elevated_border_color" => {
                            theme.elevated_border_color = map.next_value::<ColorHex>()?.0
                        }
                        "menu_item" => theme.menu_item = map.next_value::<ColorHex>()?.0,
                        "menu_item_hover" => {
                            theme.menu_item_hover = map.next_value::<ColorHex>()?.0
                        }
                        "menu_item_border_hover" => {
                            theme.menu_item_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "menu_item_active" => {
                            theme.menu_item_active = map.next_value::<ColorHex>()?.0
                        }
                        "menu_item_border_active" => {
                            theme.menu_item_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "modal_overlay_bg" => {
                            theme.modal_overlay_bg = map.next_value::<ColorHex>()?.0
                        }
                        "text_input_selection" => {
                            theme.text_input_selection = map.next_value::<ColorHex>()?.0
                        }
                        "caret_color" => theme.caret_color = map.next_value::<ColorHex>()?.0,
                        "text_highlight_background" => {
                            theme.text_highlight_background = map.next_value::<ColorHex>()?.0
                        }
                        "palette_item_hover" => {
                            theme.palette_item_hover = map.next_value::<ColorHex>()?.0
                        }
                        "palette_item_border_hover" => {
                            theme.palette_item_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "palette_item_active" => {
                            theme.palette_item_active = map.next_value::<ColorHex>()?.0
                        }
                        "palette_item_border_active" => {
                            theme.palette_item_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "scrollbar_background" => {
                            theme.scrollbar_background = map.next_value::<ColorHex>()?.0
                        }
                        "scrollbar_foreground" => {
                            theme.scrollbar_foreground = map.next_value::<ColorHex>()?.0
                        }
                        "textbox_background" => {
                            theme.textbox_background = map.next_value::<ColorHex>()?.0
                        }
                        "textbox_border" => theme.textbox_border = map.next_value::<ColorHex>()?.0,
                        "checkbox_background" => {
                            theme.checkbox_background = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_background_hover" => {
                            theme.checkbox_background_hover = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_background_active" => {
                            theme.checkbox_background_active = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_border" => {
                            theme.checkbox_border = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_border_hover" => {
                            theme.checkbox_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_border_active" => {
                            theme.checkbox_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked" => {
                            theme.checkbox_checked = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_bg" => {
                            theme.checkbox_checked_bg = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_bg_hover" => {
                            theme.checkbox_checked_bg_hover = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_bg_active" => {
                            theme.checkbox_checked_bg_active = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_border" => {
                            theme.checkbox_checked_border = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_border_hover" => {
                            theme.checkbox_checked_border_hover = map.next_value::<ColorHex>()?.0
                        }
                        "checkbox_checked_border_active" => {
                            theme.checkbox_checked_border_active = map.next_value::<ColorHex>()?.0
                        }
                        "callout_background" => {
                            theme.callout_background = map.next_value::<ColorHex>()?.0
                        }
                        "callout_border" => theme.callout_border = map.next_value::<ColorHex>()?.0,
                        "callout_text" => theme.callout_text = map.next_value::<ColorHex>()?.0,
                        "liked_song" => theme.liked_song = map.next_value::<ColorHex>()?.0,
                        "status_success" => theme.status_success = map.next_value::<ColorHex>()?.0,
                        "status_error" => theme.status_error = map.next_value::<ColorHex>()?.0,
                        "status_disabled" => {
                            theme.status_disabled = map.next_value::<ColorHex>()?.0
                        }
                        "toast_info_background" => {
                            theme.toast_info_background = map.next_value::<ColorHex>()?.0
                        }
                        "toast_info_border" => {
                            theme.toast_info_border = map.next_value::<ColorHex>()?.0
                        }
                        "toast_info_text" => {
                            theme.toast_info_text = map.next_value::<ColorHex>()?.0
                        }
                        "toast_info_track" => {
                            theme.toast_info_track = map.next_value::<ColorHex>()?.0
                        }
                        "toast_warning_background" => {
                            theme.toast_warning_background = map.next_value::<ColorHex>()?.0
                        }
                        "toast_warning_border" => {
                            theme.toast_warning_border = map.next_value::<ColorHex>()?.0
                        }
                        "toast_warning_text" => {
                            theme.toast_warning_text = map.next_value::<ColorHex>()?.0
                        }
                        "toast_warning_track" => {
                            theme.toast_warning_track = map.next_value::<ColorHex>()?.0
                        }
                        "toast_success_background" => {
                            theme.toast_success_background = map.next_value::<ColorHex>()?.0
                        }
                        "toast_success_border" => {
                            theme.toast_success_border = map.next_value::<ColorHex>()?.0
                        }
                        "toast_success_text" => {
                            theme.toast_success_text = map.next_value::<ColorHex>()?.0
                        }
                        "toast_success_track" => {
                            theme.toast_success_track = map.next_value::<ColorHex>()?.0
                        }
                        "toast_error_background" => {
                            theme.toast_error_background = map.next_value::<ColorHex>()?.0
                        }
                        "toast_error_border" => {
                            theme.toast_error_border = map.next_value::<ColorHex>()?.0
                        }
                        "toast_error_text" => {
                            theme.toast_error_text = map.next_value::<ColorHex>()?.0
                        }
                        "toast_error_track" => {
                            theme.toast_error_track = map.next_value::<ColorHex>()?.0
                        }
                        "radius_sm" => theme.radius_sm = map.next_value::<f32>()?,
                        "radius_md" => theme.radius_md = map.next_value::<f32>()?,
                        "radius_lg" => theme.radius_lg = map.next_value::<f32>()?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(theme)
            }
        }

        deserializer.deserialize_map(ThemeVisitor)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background_primary: rgb(0x0A0B0F),
            background_secondary: rgb(0x14161F),
            background_tertiary: rgb(0x1B1E2A),

            border_color: rgb(0x262A3D),

            radius_sm: 6.0,
            radius_md: 10.0,
            radius_lg: 14.0,

            album_art_background: rgb(0x303246),

            text: rgb(0xE8E9F2),
            text_secondary: rgb(0xA7ABBE),
            text_disabled: rgb(0x5F5F71),
            text_link: rgb(0x5279D4),

            nav_button_hover: rgb(0x1E2130),
            nav_button_hover_border: rgb(0x282C3F),
            nav_button_active: rgb(0x191B28),
            nav_button_active_border: rgb(0x1E2130),
            nav_button_pressed: rgb(0x242839),
            nav_button_pressed_border: rgb(0x303550),

            playback_button: rgba(0x00000000),
            playback_button_hover: rgb(0x2B3049),
            playback_button_active: rgb(0x08080B),
            playback_button_border: rgba(0x00000000),
            playback_button_toggled: rgb(0x688CF0),
            playback_button_repeat_one: rgb(0x63C58D),
            stop_after_current_indicator: rgb(0xF0A868),

            window_button: rgba(0x00000000),
            window_button_hover: rgb(0x2A3149),
            window_button_active: rgb(0x0D0F14),

            queue_item: rgba(0x00000000),
            queue_item_hover: rgb(0x181A27),
            queue_item_active: rgb(0x12141D),
            queue_item_current: rgb(0x212435),
            queue_item_selected: rgb(0x1E2748),

            close_button: rgba(0x00000000),
            close_button_hover: rgb(0x7E2C2C),
            close_button_active: rgb(0x5B1D1D),

            button_primary: rgb(0x5774E7),
            button_primary_border: rgb(0x6D85E4),
            button_primary_hover: rgb(0x6D92FF),
            button_primary_border_hover: rgb(0x5488FF),
            button_primary_active: rgb(0x495F9F),
            button_primary_border_active: rgb(0x515C8F),
            button_primary_text: rgb(0xE0E7F7),

            button_secondary: rgb(0x373B4E),
            button_secondary_border: rgb(0x4F5267),
            button_secondary_hover: rgb(0x494E67),
            button_secondary_border_hover: rgb(0x565A77),
            button_secondary_active: rgb(0x262636),
            button_secondary_border_active: rgb(0x2F3244),
            button_secondary_text: rgb(0xDDDEEC),

            button_warning: rgb(0x97792C),
            button_warning_border: rgb(0xC59E4F),
            button_warning_hover: rgb(0xA98B4A),
            button_warning_border_hover: rgb(0xC9A558),
            button_warning_active: rgb(0x5D4B2E),
            button_warning_border_active: rgb(0x80683F),
            button_warning_text: rgb(0xF0EBDE),

            button_danger: rgb(0x650B0B),
            button_danger_border: rgb(0x860808),
            button_danger_hover: rgb(0x750C0C),
            button_danger_border_hover: rgb(0x8F0B0B),
            button_danger_active: rgb(0x440A0A),
            button_danger_border_active: rgb(0x650707),
            button_danger_text: rgb(0xE9D4D4),

            slider_foreground: rgb(0x688CF0),
            slider_background: rgb(0x38374E),

            eq_grid_line: rgb(0x23283C),
            eq_grid_line_zero: rgb(0x303650),
            eq_curve: rgb(0x688CF0),
            eq_curve_fill: rgba(0x688CF02E),
            eq_band_curve: rgb(0x93ACF2),
            eq_dot: rgb(0xA0A1AD),
            eq_dot_selected: rgb(0x5774E7),
            eq_dot_disabled: rgb(0x5F5F71),
            eq_spectrum_pre: rgba(0xA0A1AD1A),
            eq_spectrum_post: rgba(0x688CF024),
            eq_spectrum_edge: rgba(0x688CF099),

            elevated_background: rgb(0x1B1E2A),
            elevated_border_color: rgb(0x2B3048),

            menu_item: rgba(0x00000000),
            menu_item_hover: rgb(0x212638),
            menu_item_border_hover: rgb(0x2F354C),
            menu_item_active: rgb(0x0E0F15),
            menu_item_border_active: rgb(0x1F212E),

            modal_overlay_bg: rgba(0x00000055),

            text_input_selection: rgba(0x01020388),
            caret_color: rgb(0xE8E8F2),
            text_highlight_background: rgba(0x3311FF30),

            palette_item_hover: rgb(0x212638),
            palette_item_border_hover: rgb(0x2F354C),
            palette_item_active: rgb(0x0E0F15),
            palette_item_border_active: rgb(0x1F212E),

            scrollbar_background: rgb(0x252839),
            scrollbar_foreground: rgb(0x616794),

            textbox_background: rgb(0x373B4E),
            textbox_border: rgb(0x4F5267),

            checkbox_background: rgb(0x373B4E),
            checkbox_background_hover: rgb(0x494E67),
            checkbox_background_active: rgb(0x262636),
            checkbox_border: rgb(0x4F5267),
            checkbox_border_hover: rgb(0x565A77),
            checkbox_border_active: rgb(0x2F3244),
            checkbox_checked: rgb(0xC7C7D8),
            checkbox_checked_bg: rgb(0x618EE6),
            checkbox_checked_bg_hover: rgb(0x6080F9),
            checkbox_checked_bg_active: rgb(0x495D9F),
            checkbox_checked_border: rgb(0x7592E7),
            checkbox_checked_border_hover: rgb(0x657DFF),
            checkbox_checked_border_active: rgb(0x515D8F),

            callout_background: rgba(0x2E280053),
            callout_border: rgba(0x5B45008E),
            callout_text: rgb(0xF0EBDE),

            liked_song: rgb(0x688CF0),

            status_success: rgb(0x63C58D),
            status_error: rgb(0xE54D4D),
            status_disabled: rgb(0x5F5F71),

            toast_info_background: rgb(0x1B1E2A),
            toast_info_border: rgb(0x2B3048),
            toast_info_text: rgb(0xE8E9F2),
            toast_info_track: rgb(0xA0A1AD),

            toast_warning_background: rgb(0x18160C),
            toast_warning_border: rgb(0x3D3005),
            toast_warning_text: rgb(0xF0EBDE),
            toast_warning_track: rgb(0xB5B570),

            toast_success_background: rgb(0x121F11),
            toast_success_border: rgb(0x0C3D05),
            toast_success_text: rgb(0xEAF2E8),
            toast_success_track: rgb(0x74A677),

            toast_error_background: rgb(0x291817),
            toast_error_border: rgb(0x4d2422),
            toast_error_text: rgb(0xF2E8E8),
            toast_error_track: rgb(0xC27F7A),
        }
    }
}

impl Global for Theme {}

/// 内建亮色主题的 settings 标识，无需主题文件即可选择。
pub const LIGHT_THEME_ID: &str = "light";

impl Theme {
    /// 亮色主题：以 #FBFAFA 为主背景色，与暗色默认主题一一对应。
    pub fn light() -> Self {
        Self {
            background_primary: rgb(0xF8F6F2),
            background_secondary: rgb(0xEFECE4),
            background_tertiary: rgb(0xE6E2D8),

            border_color: rgb(0xE0DACE),

            radius_sm: 6.0,
            radius_md: 10.0,
            radius_lg: 14.0,

            album_art_background: rgb(0xE2DDD2),

            text: rgb(0x262420),
            text_secondary: rgb(0x6E6A5F),
            text_disabled: rgb(0xB5B0A3),
            text_link: rgb(0x3462D0),

            nav_button_hover: rgb(0xEFECE3),
            nav_button_hover_border: rgb(0xE0DBCE),
            nav_button_active: rgb(0xE9E5D9),
            nav_button_active_border: rgb(0xDAD4C4),
            nav_button_pressed: rgb(0xECE8DD),
            nav_button_pressed_border: rgb(0xDDD7C8),

            playback_button: rgba(0x00000000),
            playback_button_hover: rgb(0xE9E5D9),
            playback_button_active: rgb(0xD6D0C0),
            playback_button_border: rgba(0x00000000),
            playback_button_toggled: rgb(0x3B63E8),
            playback_button_repeat_one: rgb(0x2FA96B),
            stop_after_current_indicator: rgb(0xE08A3C),

            window_button: rgba(0x00000000),
            window_button_hover: rgb(0xEAE6DB),
            window_button_active: rgb(0xD8D2C3),

            queue_item: rgba(0x00000000),
            queue_item_hover: rgb(0xEFEBE0),
            queue_item_active: rgb(0xE9E5D9),
            queue_item_current: rgb(0xECE8DC),
            queue_item_selected: rgb(0xD9E3FA),

            close_button: rgba(0x00000000),
            close_button_hover: rgb(0xD05555),
            close_button_active: rgb(0xA83A3A),

            button_primary: rgb(0x3B63E8),
            button_primary_border: rgb(0x5577F2),
            button_primary_hover: rgb(0x5275F2),
            button_primary_border_hover: rgb(0x6485FF),
            button_primary_active: rgb(0x2E4FBF),
            button_primary_border_active: rgb(0x3D58C5),
            button_primary_text: rgb(0xFFFFFF),

            button_secondary: rgb(0xEAE6DB),
            button_secondary_border: rgb(0xD8D2C2),
            button_secondary_hover: rgb(0xE1DCCE),
            button_secondary_border_hover: rgb(0xCBC4B2),
            button_secondary_active: rgb(0xD6D0C0),
            button_secondary_border_active: rgb(0xBFB8A6),
            button_secondary_text: rgb(0x3C3A30),

            button_warning: rgb(0xA87F22),
            button_warning_border: rgb(0xC49A3F),
            button_warning_hover: rgb(0xB88D33),
            button_warning_border_hover: rgb(0xCEA449),
            button_warning_active: rgb(0x86651D),
            button_warning_border_active: rgb(0x9C7730),
            button_warning_text: rgb(0xFFFFFF),

            button_danger: rgb(0xB23A32),
            button_danger_border: rgb(0xD0544C),
            button_danger_hover: rgb(0xC1473E),
            button_danger_border_hover: rgb(0xD86058),
            button_danger_active: rgb(0x8E2B25),
            button_danger_border_active: rgb(0xA63A33),
            button_danger_text: rgb(0xFFFFFF),

            slider_foreground: rgb(0x3B63E8),
            slider_background: rgb(0xE0DBCE),

            eq_grid_line: rgb(0xE4DFD2),
            eq_grid_line_zero: rgb(0xD2CCBC),
            eq_curve: rgb(0x3B63E8),
            eq_curve_fill: rgba(0x3B63E81F),
            eq_band_curve: rgb(0x6F88E0),
            eq_dot: rgb(0x6E6A5F),
            eq_dot_selected: rgb(0x3B63E8),
            eq_dot_disabled: rgb(0xB2B1BD),
            eq_spectrum_pre: rgba(0x6A6A7720),
            eq_spectrum_post: rgba(0x3B63E82E),
            eq_spectrum_edge: rgba(0x3B63E899),

            elevated_background: rgb(0xFDFCFA),
            elevated_border_color: rgb(0xE3DED1),

            menu_item: rgba(0x00000000),
            menu_item_hover: rgb(0xEFECE3),
            menu_item_border_hover: rgb(0xE1DCCC),
            menu_item_active: rgb(0xE9E5D9),
            menu_item_border_active: rgb(0xDAD4C4),

            modal_overlay_bg: rgba(0x00000033),

            text_input_selection: rgba(0x3B63E844),
            caret_color: rgb(0x262420),
            text_highlight_background: rgba(0x3B63E838),

            palette_item_hover: rgb(0xEFECE3),
            palette_item_border_hover: rgb(0xE1DCCC),
            palette_item_active: rgb(0xE9E5D9),
            palette_item_border_active: rgb(0xDAD4C4),

            scrollbar_background: rgb(0xE9E5DB),
            scrollbar_foreground: rgb(0xC7C0AF),

            textbox_background: rgb(0xFFFFFF),
            textbox_border: rgb(0xD9D3C4),

            checkbox_background: rgb(0xFFFFFF),
            checkbox_background_hover: rgb(0xF6F4EE),
            checkbox_background_active: rgb(0xECE8DC),
            checkbox_border: rgb(0xCCC6B6),
            checkbox_border_hover: rgb(0xB6B0A0),
            checkbox_border_active: rgb(0xA69F8E),
            checkbox_checked: rgb(0xFFFFFF),
            checkbox_checked_bg: rgb(0x3B63E8),
            checkbox_checked_bg_hover: rgb(0x5275F2),
            checkbox_checked_bg_active: rgb(0x2E4FBF),
            checkbox_checked_border: rgb(0x3B63E8),
            checkbox_checked_border_hover: rgb(0x5275F2),
            checkbox_checked_border_active: rgb(0x2E4FBF),

            callout_background: rgba(0xFFE9B0B8),
            callout_border: rgba(0xB98A2E99),
            callout_text: rgb(0x4A3A12),

            liked_song: rgb(0x3B63E8),

            status_success: rgb(0x2FA96B),
            status_error: rgb(0xD64545),
            status_disabled: rgb(0xB2B1BD),

            toast_info_background: rgb(0xFDFCFA),
            toast_info_border: rgb(0xE4DFD2),
            toast_info_text: rgb(0x262420),
            toast_info_track: rgb(0x6E6A5F),

            toast_warning_background: rgb(0xFFF6E3),
            toast_warning_border: rgb(0xE4C98A),
            toast_warning_text: rgb(0x4A3A12),
            toast_warning_track: rgb(0xA87F22),

            toast_success_background: rgb(0xE8F6EC),
            toast_success_border: rgb(0xA9D9B8),
            toast_success_text: rgb(0x14401F),
            toast_success_track: rgb(0x2FA96B),

            toast_error_background: rgb(0xFCE9E8),
            toast_error_border: rgb(0xE5A8A4),
            toast_error_text: rgb(0x5C1A16),
            toast_error_track: rgb(0xD64545),
        }
    }
}

/// Legacy single-file theme read from the data directory root.
pub const LEGACY_THEME_PATH: &str = "theme.json";
/// Data-directory subdirectory holding one `<name>.json` theme per file.
pub const THEMES_DIR_NAME: &str = "themes";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeOption {
    pub id: Option<String>,
    pub label: String,
}

pub struct ThemeOptionsGlobal {
    pub model: Entity<Vec<ThemeOption>>,
}

impl Global for ThemeOptionsGlobal {}

pub fn create_theme(path: &Path) -> Theme {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            warn!("Theme file could not be opened, using default: {:?}", e);
            return Theme::default();
        }
    };

    let reader = BufReader::new(file);
    match serde_json::from_reader(reader) {
        Ok(theme) => theme,
        Err(e) => {
            warn!(
                "Theme file exists but it could not be loaded, using default: {:?}",
                e
            );
            Theme::default()
        }
    }
}

/// Discovers all available theme options in the data directory.
/// Returns a vector containing the default theme, legacy theme (if present),
/// and any custom themes found in the themes subdirectory.
pub fn discover_theme_options(data_dir: &Path) -> Vec<ThemeOption> {
    let mut themes = vec![
        ThemeOption {
            id: None,
            label: "Default".to_string(),
        },
        ThemeOption {
            id: Some(LIGHT_THEME_ID.to_string()),
            label: "Light".to_string(),
        },
    ];

    let legacy_theme = data_dir.join(LEGACY_THEME_PATH);
    if legacy_theme.is_file() {
        themes.push(ThemeOption {
            id: Some(LEGACY_THEME_PATH.to_string()),
            label: "Legacy".to_string(),
        });
    }

    let themes_dir = data_dir.join(THEMES_DIR_NAME);
    let mut custom_themes = fs::read_dir(themes_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .filter_map(|path| {
            let file_name = path.file_name()?.to_string_lossy().into_owned();
            let label = file_name
                .strip_suffix(".json")
                .map(|s| s.to_string())
                .unwrap_or(file_name.clone());
            Some(ThemeOption {
                id: Some(format!("{THEMES_DIR_NAME}/{file_name}")),
                label,
            })
        })
        .collect::<Vec<_>>();

    custom_themes.sort_by(|a, b| a.id.cmp(&b.id));
    themes.extend(custom_themes);
    themes
}

/// Resolves a theme identifier to its relative path if the file exists.
/// Returns None if no theme is selected or the file does not exist.
pub fn resolve_theme_relative_path(
    data_dir: &Path,
    selected_theme: Option<&str>,
) -> Option<String> {
    if let Some(selected_theme) = selected_theme {
        let path = data_dir.join(selected_theme);
        return path.is_file().then(|| selected_theme.to_string());
    }

    None
}

/// Loads the theme for the given selection, falling back to the default theme
/// if the file does not exist or cannot be parsed.
pub fn load_selected_theme(data_dir: &Path, selected_theme: Option<&str>) -> Theme {
    if selected_theme == Some(LIGHT_THEME_ID) {
        return Theme::light();
    }

    resolve_theme_relative_path(data_dir, selected_theme)
        .map(|path| data_dir.join(path))
        .map(|path| create_theme(&path))
        .unwrap_or_default()
}

/// Converts a filesystem path to a theme-relative path for comparison.
fn theme_relative_path_for_event(data_dir: &Path, path: &Path) -> Option<String> {
    if path.parent() == Some(data_dir) && path.file_name() == Some(LEGACY_THEME_PATH.as_ref()) {
        return Some(LEGACY_THEME_PATH.to_string());
    }

    let themes_dir = data_dir.join(THEMES_DIR_NAME);
    if path.parent() == Some(themes_dir.as_path())
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        let file_name = path.file_name()?.to_string_lossy();
        return Some(format!("{THEMES_DIR_NAME}/{file_name}"));
    }

    None
}

/// Checks if any of the paths in a filesystem event affect the currently selected theme.
///
/// A plain string compare against the selected id suffices:
/// `resolve_theme_relative_path` either echoes the selected id back or
/// returns `None`, so resolving it here adds nothing.
fn event_affects_selected_theme(
    data_dir: &Path,
    selected_theme: Option<&str>,
    event_paths: &[PathBuf],
) -> bool {
    let Some(selected_theme) = selected_theme else {
        return false;
    };

    event_paths
        .iter()
        .filter_map(|path| theme_relative_path_for_event(data_dir, path))
        .any(|changed_path| changed_path == selected_theme)
}

/// Checks whether a filesystem event changes the set of available theme choices.
fn event_affects_theme_options(data_dir: &Path, event_paths: &[PathBuf]) -> bool {
    let themes_dir = data_dir.join(THEMES_DIR_NAME);

    event_paths
        .iter()
        .any(|path| path == &themes_dir || theme_relative_path_for_event(data_dir, path).is_some())
}

#[derive(PartialEq, Clone)]
pub struct ThemeEvTransmitter;

impl EventEmitter<Theme> for ThemeEvTransmitter {}

#[allow(dead_code)]
pub struct ThemeWatcher(pub Box<dyn Watcher>);

impl Global for ThemeWatcher {}

pub fn setup_theme(cx: &mut App, data_dir: PathBuf) {
    let settings_model = cx.global::<SettingsGlobal>().model.clone();
    let selected_theme = settings_model.read(cx).interface.theme.clone();
    let selected_theme_state = Arc::new(RwLock::new(selected_theme.clone()));
    let theme_options_model = cx.new({
        let data_dir = data_dir.clone();
        move |_| discover_theme_options(&data_dir)
    });

    cx.set_global(ThemeOptionsGlobal {
        model: theme_options_model.clone(),
    });

    cx.set_global(load_selected_theme(&data_dir, selected_theme.as_deref()));
    let theme_transmitter = cx.new(|_| ThemeEvTransmitter);

    cx.subscribe(&theme_transmitter, |_, theme, cx| {
        cx.set_global(theme.clone());
        cx.refresh_windows();
    })
    .detach();

    let data_dir_for_settings = data_dir.clone();
    let selected_theme_state_for_settings = selected_theme_state.clone();
    let theme_transmitter_for_settings = theme_transmitter.clone();
    let settings_model_for_observer = settings_model.clone();
    cx.observe(&settings_model, move |_, cx| {
        let selected_theme = settings_model_for_observer.read(cx).interface.theme.clone();
        let should_update = {
            let mut current_theme = selected_theme_state_for_settings
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if *current_theme == selected_theme {
                false
            } else {
                *current_theme = selected_theme.clone();
                true
            }
        };

        if should_update {
            let theme = load_selected_theme(&data_dir_for_settings, selected_theme.as_deref());
            theme_transmitter_for_settings.update(cx, move |_, m| {
                m.emit(theme);
            });
        }
    })
    .detach();

    let (tx, rx) = channel::<notify::Result<Event>>();
    let watcher = notify::recommended_watcher(tx);

    if let Ok(mut watcher) = watcher {
        if let Err(e) = watcher.watch(&data_dir, RecursiveMode::Recursive) {
            warn!("failed to watch theme directory: {:?}", e);
        }

        cx.spawn({
            let data_dir = data_dir.clone();
            let selected_theme_state = selected_theme_state.clone();
            let theme_transmitter = theme_transmitter.clone();
            let theme_options_model = theme_options_model.clone();
            async move |cx: &mut AsyncApp| {
                loop {
                    while let Ok(event) = rx.try_recv() {
                        match event {
                            Ok(v) => match v.kind {
                                notify::EventKind::Create(_)
                                | notify::EventKind::Modify(_)
                                | notify::EventKind::Remove(_) => {
                                    if event_affects_theme_options(&data_dir, &v.paths) {
                                        let theme_options = discover_theme_options(&data_dir);
                                        theme_options_model.update(cx, move |current, cx| {
                                            if *current != theme_options {
                                                *current = theme_options;
                                            }
                                            cx.notify();
                                        });
                                    }

                                    let selected_theme = selected_theme_state
                                        .read()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .clone();
                                    if !event_affects_selected_theme(
                                        &data_dir,
                                        selected_theme.as_deref(),
                                        &v.paths,
                                    ) {
                                        continue;
                                    }

                                    info!("Theme changed, updating...");
                                    let theme =
                                        load_selected_theme(&data_dir, selected_theme.as_deref());
                                    theme_transmitter.update(cx, move |_, m| {
                                        m.emit(theme);
                                    });
                                }
                                _ => (),
                            },
                            Err(e) => error!("error occurred while watching themes: {:?}", e),
                        }
                    }

                    // Theme/settings files are hand-edited JSON; a 1 s poll
                    // reacts within perception while keeping these background
                    // wake-ups rare.
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                }
            }
        })
        .detach();

        // store the watcher in a global so it doesn't go out of scope
        let tw = ThemeWatcher(Box::new(watcher));
        cx.set_global(tw);
    } else if let Err(e) = watcher {
        warn!("failed to watch theme directory: {:?}", e);
    }
}
