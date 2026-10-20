use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, mpsc::channel},
    time::Duration,
};

use crate::settings::SettingsGlobal;
use crate::sync::RecoveredRwLock;
use gpui::{App, AppContext, AsyncApp, Entity, EventEmitter, Global, Rgba, rgb, rgba};
use notify::{Event, RecursiveMode, Watcher};
use serde::{
    Deserialize, Deserializer,
    de::{Error as SerdeError, IgnoredAny, MapAccess, Visitor},
};
use tracing::{error, info, warn};

/// 从主题文件里那种 CSS 风格的十六进制字符串解析出的颜色。
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

#[derive(Clone, Debug)]
pub struct Theme {
    pub background_primary: Rgba,
    pub background_secondary: Rgba,
    pub background_tertiary: Rgba,

    pub border_color: Rgba,

    /// 圆角尺度，单位 px，供组件消费，取代写死的 px() 值。
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
                // 主题字段清一色是「JSON 键 == 结构体字段名 == Rgba」这一形态。逐条
                // 手写 match 分支就是维护 129 份复制粘贴，而漏掉一条不会报错：未知键
                // 被最后一支当噪音吃掉，该颜色静默停在默认值。宏把键名与字段名成对列
                // 在唯一一处，新增主题色只加一行。
                // theme/map/key 由调用点传入：宏体内的自由标识符按定义点解析，
                // `key` 声明在 while 的子作用域里，不传进来就找不到。
                macro_rules! color_arms {
                    ($theme:ident, $map:ident, $key:expr; $($name:literal => $field:ident),* $(,)?) => {
                        match $key {
                            $($name => $theme.$field = $map.next_value::<ColorHex>()?.0,)*
                            "radius_sm" => $theme.radius_sm = $map.next_value::<f32>()?,
                            "radius_md" => $theme.radius_md = $map.next_value::<f32>()?,
                            "radius_lg" => $theme.radius_lg = $map.next_value::<f32>()?,
                            _ => {
                                $map.next_value::<IgnoredAny>()?;
                            }
                        }
                    };
                }

                let mut theme = Theme::default();
                while let Some(key) = map.next_key::<String>()? {
                    color_arms!(theme, map, key.as_str();
                        "background_primary" => background_primary,
                        "background_secondary" => background_secondary,
                        "background_tertiary" => background_tertiary,
                        "border_color" => border_color,
                        "album_art_background" => album_art_background,
                        "text" => text,
                        "text_secondary" => text_secondary,
                        "text_disabled" => text_disabled,
                        "text_link" => text_link,
                        "nav_button_hover" => nav_button_hover,
                        "nav_button_hover_border" => nav_button_hover_border,
                        "nav_button_active" => nav_button_active,
                        "nav_button_active_border" => nav_button_active_border,
                        "nav_button_pressed" => nav_button_pressed,
                        "nav_button_pressed_border" => nav_button_pressed_border,
                        "playback_button" => playback_button,
                        "playback_button_hover" => playback_button_hover,
                        "playback_button_active" => playback_button_active,
                        "playback_button_border" => playback_button_border,
                        "playback_button_toggled" => playback_button_toggled,
                        "playback_button_repeat_one" => playback_button_repeat_one,
                        "stop_after_current_indicator" => stop_after_current_indicator,
                        "window_button" => window_button,
                        "window_button_hover" => window_button_hover,
                        "window_button_active" => window_button_active,
                        "close_button" => close_button,
                        "close_button_hover" => close_button_hover,
                        "close_button_active" => close_button_active,
                        "queue_item" => queue_item,
                        "queue_item_hover" => queue_item_hover,
                        "queue_item_active" => queue_item_active,
                        "queue_item_current" => queue_item_current,
                        "queue_item_selected" => queue_item_selected,
                        "button_primary" => button_primary,
                        "button_primary_border" => button_primary_border,
                        "button_primary_hover" => button_primary_hover,
                        "button_primary_border_hover" => button_primary_border_hover,
                        "button_primary_active" => button_primary_active,
                        "button_primary_border_active" => button_primary_border_active,
                        "button_primary_text" => button_primary_text,
                        "button_secondary" => button_secondary,
                        "button_secondary_border" => button_secondary_border,
                        "button_secondary_hover" => button_secondary_hover,
                        "button_secondary_border_hover" => button_secondary_border_hover,
                        "button_secondary_active" => button_secondary_active,
                        "button_secondary_border_active" => button_secondary_border_active,
                        "button_secondary_text" => button_secondary_text,
                        "button_warning" => button_warning,
                        "button_warning_border" => button_warning_border,
                        "button_warning_hover" => button_warning_hover,
                        "button_warning_border_hover" => button_warning_border_hover,
                        "button_warning_active" => button_warning_active,
                        "button_warning_border_active" => button_warning_border_active,
                        "button_warning_text" => button_warning_text,
                        "button_danger" => button_danger,
                        "button_danger_border" => button_danger_border,
                        "button_danger_hover" => button_danger_hover,
                        "button_danger_border_hover" => button_danger_border_hover,
                        "button_danger_active" => button_danger_active,
                        "button_danger_border_active" => button_danger_border_active,
                        "button_danger_text" => button_danger_text,
                        "slider_foreground" => slider_foreground,
                        "slider_background" => slider_background,
                        "eq_grid_line" => eq_grid_line,
                        "eq_grid_line_zero" => eq_grid_line_zero,
                        "eq_curve" => eq_curve,
                        "eq_curve_fill" => eq_curve_fill,
                        "eq_band_curve" => eq_band_curve,
                        "eq_dot" => eq_dot,
                        "eq_dot_selected" => eq_dot_selected,
                        "eq_dot_disabled" => eq_dot_disabled,
                        "eq_spectrum_pre" => eq_spectrum_pre,
                        "eq_spectrum_post" => eq_spectrum_post,
                        "eq_spectrum_edge" => eq_spectrum_edge,
                        "elevated_background" => elevated_background,
                        "elevated_border_color" => elevated_border_color,
                        "menu_item" => menu_item,
                        "menu_item_hover" => menu_item_hover,
                        "menu_item_border_hover" => menu_item_border_hover,
                        "menu_item_active" => menu_item_active,
                        "menu_item_border_active" => menu_item_border_active,
                        "modal_overlay_bg" => modal_overlay_bg,
                        "text_input_selection" => text_input_selection,
                        "caret_color" => caret_color,
                        "text_highlight_background" => text_highlight_background,
                        "palette_item_hover" => palette_item_hover,
                        "palette_item_border_hover" => palette_item_border_hover,
                        "palette_item_active" => palette_item_active,
                        "palette_item_border_active" => palette_item_border_active,
                        "scrollbar_background" => scrollbar_background,
                        "scrollbar_foreground" => scrollbar_foreground,
                        "textbox_background" => textbox_background,
                        "textbox_border" => textbox_border,
                        "checkbox_background" => checkbox_background,
                        "checkbox_background_hover" => checkbox_background_hover,
                        "checkbox_background_active" => checkbox_background_active,
                        "checkbox_border" => checkbox_border,
                        "checkbox_border_hover" => checkbox_border_hover,
                        "checkbox_border_active" => checkbox_border_active,
                        "checkbox_checked" => checkbox_checked,
                        "checkbox_checked_bg" => checkbox_checked_bg,
                        "checkbox_checked_bg_hover" => checkbox_checked_bg_hover,
                        "checkbox_checked_bg_active" => checkbox_checked_bg_active,
                        "checkbox_checked_border" => checkbox_checked_border,
                        "checkbox_checked_border_hover" => checkbox_checked_border_hover,
                        "checkbox_checked_border_active" => checkbox_checked_border_active,
                        "callout_background" => callout_background,
                        "callout_border" => callout_border,
                        "callout_text" => callout_text,
                        "liked_song" => liked_song,
                        "status_success" => status_success,
                        "status_error" => status_error,
                        "status_disabled" => status_disabled,
                        "toast_info_background" => toast_info_background,
                        "toast_info_border" => toast_info_border,
                        "toast_info_text" => toast_info_text,
                        "toast_info_track" => toast_info_track,
                        "toast_warning_background" => toast_warning_background,
                        "toast_warning_border" => toast_warning_border,
                        "toast_warning_text" => toast_warning_text,
                        "toast_warning_track" => toast_warning_track,
                        "toast_success_background" => toast_success_background,
                        "toast_success_border" => toast_success_border,
                        "toast_success_text" => toast_success_text,
                        "toast_success_track" => toast_success_track,
                        "toast_error_background" => toast_error_background,
                        "toast_error_border" => toast_error_border,
                        "toast_error_text" => toast_error_text,
                        "toast_error_track" => toast_error_track,
                    );
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

/// 从数据目录根读到的旧版单文件主题。
pub const LEGACY_THEME_PATH: &str = "theme.json";
/// 数据目录下的子目录，每个 `<name>.json` 文件是一个主题。
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

/// 发现数据目录里所有可用的主题选项。返回的 vector 含默认主题、旧版主题
/// （如果存在），以及 themes 子目录里找到的任何自定义主题。
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

/// 文件存在时把主题标识解析成它的相对路径。没选中主题或文件不存在时返回 None。
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

/// 按给定的选择加载主题，文件不存在或解析不了时回退到默认主题。
pub fn load_selected_theme(data_dir: &Path, selected_theme: Option<&str>) -> Theme {
    if selected_theme == Some(LIGHT_THEME_ID) {
        return Theme::light();
    }

    resolve_theme_relative_path(data_dir, selected_theme)
        .map(|path| data_dir.join(path))
        .map(|path| create_theme(&path))
        .unwrap_or_default()
}

/// 把文件系统路径转换成主题相对路径，以便比较。
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

/// 检查事件的哪些主题相对路径影响当前选中的主题。
///
/// 拿选中 id 做一次普通字符串比较就够了：
/// `resolve_theme_relative_path` 要么把选中的 id 原样回吐，要么返回 `None`，
/// 所以在这里再解析它不会多给任何东西。
fn event_affects_selected_theme(selected_theme: Option<&str>, affected: &[String]) -> bool {
    let Some(selected_theme) = selected_theme else {
        return false;
    };

    affected
        .iter()
        .any(|affected_path| affected_path == selected_theme)
}

/// 检查某个文件系统事件是否改变了可用主题的集合：themes 目录本身被碰过，
/// 或某个主题文件匹配上了。
fn event_affects_theme_options(themes_dir_touched: bool, affected: &[String]) -> bool {
    themes_dir_touched || !affected.is_empty()
}

#[derive(PartialEq, Clone)]
pub struct ThemeEvTransmitter;

impl EventEmitter<Theme> for ThemeEvTransmitter {}

/// 字段本身从不被读：把它放进 global 只为让 `Box<dyn Watcher>` 随进程存活，
/// 一旦被替换或移除，主题目录的监听就静默停止。
#[allow(dead_code)]
pub struct ThemeWatcher(pub Box<dyn Watcher>);

impl Global for ThemeWatcher {}

/// 发布主题相关的 Global（选项列表 + 选中主题）并 watch 主题目录。主题目录
/// 枚举已在别处完成：启动预取在 tokio 的 spawn_blocking 上并行做（见
/// `ui/app.rs` 的启动序列）。选中主题的文件读取（`load_selected_theme`，
/// 单个小 JSON）保持同步——首帧前就需要 Theme，且它依赖设置加载的结果，
/// 无法再往前并行。
pub fn setup_theme_with_options(cx: &mut App, data_dir: PathBuf, theme_options: Vec<ThemeOption>) {
    let settings_model = cx.global::<SettingsGlobal>().model.clone();
    let selected_theme = settings_model.read(cx).interface.theme.clone();
    let selected_theme_state = Arc::new(RwLock::new(selected_theme.clone()));
    let theme_options_model = cx.new(|_| theme_options);

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
            let mut current_theme = selected_theme_state_for_settings.write_recovered();
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
    let themes_dir = data_dir.join(THEMES_DIR_NAME);

    if let Ok(mut watcher) = watcher {
        // themes/ 可能尚不存在（首次安装且未点过"打开主题文件夹"），而
        // NonRecursive watch 要求目录已在；先补建以保住主题文件热重载覆盖。
        let _ = fs::create_dir_all(&themes_dir);
        // 两个 NonRecursive watch：根目录只为 legacy theme.json，themes/ 为
        // 主题 JSON——image-cache / WAL 等子目录的高频写盘在事件源外即被挡掉。
        if let Err(e) = watcher.watch(&data_dir, RecursiveMode::NonRecursive) {
            warn!("failed to watch theme directory: {:?}", e);
        }
        if let Err(e) = watcher.watch(&themes_dir, RecursiveMode::NonRecursive) {
            warn!("failed to watch themes directory: {:?}", e);
        }

        cx.spawn({
            let data_dir = data_dir.clone();
            let themes_dir = themes_dir.clone();
            let theme_transmitter = theme_transmitter.clone();
            async move |cx: &mut AsyncApp| {
                loop {
                    while let Ok(event) = rx.try_recv() {
                        match event {
                            Ok(v) => match v.kind {
                                notify::EventKind::Create(_)
                                | notify::EventKind::Modify(_)
                                | notify::EventKind::Remove(_) => {
                                    // 事件路径的相对化整批只算一次，两处判定共用。
                                    let affected: Vec<String> = v
                                        .paths
                                        .iter()
                                        .filter_map(|path| {
                                            theme_relative_path_for_event(&data_dir, path)
                                        })
                                        .collect();
                                    let themes_dir_touched =
                                        v.paths.iter().any(|path| path == &themes_dir);

                                    if event_affects_theme_options(themes_dir_touched, &affected) {
                                        let theme_options = discover_theme_options(&data_dir);
                                        theme_options_model.update(cx, move |current, cx| {
                                            if *current != theme_options {
                                                *current = theme_options;
                                            }
                                            cx.notify();
                                        });
                                    }

                                    let selected_theme =
                                        selected_theme_state.read_recovered().clone();
                                    if !event_affects_selected_theme(
                                        selected_theme.as_deref(),
                                        &affected,
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

                    // 主题/设置文件是手改的 JSON；1 s 轮询能在知觉阈值内响应，
                    // 同时让这些后台唤醒保持稀少。
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                }
            }
        })
        .detach();

        // 把 watcher 存进一个 global，免得它离开作用域就被析构
        let tw = ThemeWatcher(Box::new(watcher));
        cx.set_global(tw);
    } else if let Err(e) = watcher {
        warn!("failed to watch theme directory: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把宏体里那份「键名 => 字段名」清单当数据取出来，两个测试共用。
    fn deserialization_arms() -> Vec<(String, String)> {
        let source = include_str!("theme.rs");
        let mapping = source
            .split("macro_rules! color_arms")
            .nth(1)
            .and_then(|tail| tail.split("deserializer.deserialize_map").next())
            .expect("键清单段落");

        let mut arms = Vec::new();
        for line in mapping.lines() {
            let Some(rest) = line.trim().strip_prefix('"') else {
                continue;
            };
            let Some((key, rest)) = rest.split_once('"') else {
                continue;
            };
            let Some(arm) = rest.trim().strip_prefix("=>") else {
                continue;
            };
            let field = arm
                .split('=')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_end_matches(',')
                .rsplit('.')
                .next()
                .unwrap_or_default();
            arms.push((key.to_string(), field.to_string()));
        }
        arms
    }

    /// 主题反序列化的命名契约：`Theme` 的每个字段都要在键清单里出现恰好一次，
    /// 且形态固定为「键名 == 字段名」。少一条不会有任何运行时迹象——未知键被宏的
    /// 忽略支吃掉，那个颜色就永远停在默认值。
    #[test]
    fn every_theme_field_has_exactly_one_deserialization_key() {
        let arms = deserialization_arms();
        for (key, field) in &arms {
            assert_eq!(key, field, "键名必须与字段名同名，分支：{key} => {field}");
        }
        let mut keys: Vec<String> = arms.iter().map(|(key, _)| key.clone()).collect();

        let source = include_str!("theme.rs");
        let struct_body = source
            .split("pub struct Theme {")
            .nth(1)
            .and_then(|tail| tail.split("\n}").next())
            .expect("Theme 结构体");
        let mut fields: Vec<String> = Vec::new();
        for line in struct_body.lines() {
            let Some(rest) = line.trim().strip_prefix("pub ") else {
                continue;
            };
            let Some((name, ty)) = rest.split_once(':') else {
                continue;
            };
            if matches!(ty.trim().trim_end_matches(','), "Rgba" | "f32") {
                fields.push(name.trim().to_string());
            }
        }

        assert!(
            fields.len() > 100,
            "字段扫描明显不对，只找到 {}",
            fields.len()
        );
        keys.sort();
        fields.sort();
        assert_eq!(fields, keys, "Theme 字段与反序列化键必须一一对应");
    }

    /// 行为面：文本核对只能证明"键名与字段名同名"，证明不了宏展开出来的赋值
    /// 真的发生（写错目标字段、少一个 `=`，键名照样对得上）。这里把清单里每个键
    /// 都赋成同一个醒目值真解析一遍，再数这些值在结果里出现的次数。
    #[test]
    fn every_deserialization_key_actually_assigns_its_field() {
        const PROBE_HEX: &str = "#01020304";
        const PROBE_RADIUS: f32 = 7.5;

        let probe = format!("{:?}", parse_hex_color(PROBE_HEX).expect("探针色应能解析"));
        let mut doc = serde_json::Map::new();
        let mut color_keys = 0usize;
        for (key, _) in deserialization_arms() {
            if key.starts_with("radius_") {
                doc.insert(key, serde_json::Value::from(PROBE_RADIUS));
            } else {
                doc.insert(key, serde_json::Value::from(PROBE_HEX));
                color_keys += 1;
            }
        }
        // 未知键必须被忽略支安静吃掉，不能让整个文档解析失败。
        doc.insert(
            "definitely_not_a_theme_key".into(),
            serde_json::json!(PROBE_HEX),
        );

        let theme: Theme =
            serde_json::from_value(serde_json::Value::Object(doc)).expect("整份键清单应能解析");
        let rendered = format!("{theme:?}");
        let assigned = rendered.matches(probe.as_str()).count();

        assert_eq!(
            assigned,
            color_keys,
            "有 {} 个色键没被真的赋值（宏分支缺失或写错了目标字段）",
            color_keys - assigned
        );
        assert_eq!(
            rendered.matches(&format!("{PROBE_RADIUS}")).count(),
            3,
            "三个 radius_* 都应落到 f32 字段上"
        );
    }
}
