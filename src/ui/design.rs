//! Design tokens for the whole UI.
//!
//! Everything under `ui/` that expresses space, type size or icon size should
//! go through this module instead of writing a raw value at the call site.
//!
//! # Why this exists
//!
//! Before this module the UI carried **19 distinct spacing values**, **9
//! distinct icon sizes** and **16 raw font sizes** on top of GPUI's own text
//! scale. Most of them sat one or two pixels away from the nearest round
//! number: `px(6.0)` next to `px(8.0)`, `px(13.0)` next to `text_sm()` (14px),
//! `px(11.0)` next to `text_xs()` (12px). No single one of those is wrong,
//! which is exactly why they accumulated - each looked like a reasonable local
//! adjustment at the time. Together they are why the UI reads as "rough": the
//! eye cannot name a 1px discrepancy, it just refuses to call the result
//! polished.
//!
//! # The rules
//!
//! - **Spacing** - 4pt grid. Snap to the nearest step below; never invent a
//!   value because a neighbour happens to look tight.
//! - **Type** - only GPUI's `text_xs()` / `text_sm()` / `text_base()` /
//!   `text_lg()` / `text_xl()` (12 / 14 / 16 / 18 / 20px). They are `rem`-based
//!   and so follow the root font size; a raw `text_size(px(…))` does not, which
//!   is a second reason not to write one.
//! - **Icons** - three sizes only. `icon()` deliberately has no default size so
//!   each call site has to pick: inline ([`ICON_SM`]), standard ([`ICON_MD`]),
//!   or primary ([`ICON_LG`]).
//! - **Radius** - `Theme::radius_sm` / `radius_md` / `radius_lg`, never a raw
//!   `rounded(px(…))`.
//!
//! `ui::components::label` is the reference implementation of the settings-row
//! rhythm built on these.
//!
//! A scale is only useful if it is complete, so the full set lives here whether
//! or not every step has a caller yet: migrating the remaining call sites will
//! pull them in, and a half-populated scale invites inventing the missing step
//! by hand - the exact failure this module exists to stop.
#![allow(dead_code)]

use gpui::{Pixels, px};

// ----------------------------------------------------------------- spacing --

/// Optical nudges inside a single glyph or icon. Not a layout value: if you are
/// reaching for this to separate two elements, you want [`SPACE_XS`].
pub const SPACE_HAIRLINE: Pixels = px(2.0);

/// Tight: closely related parts of one row (a title and its trailing badge).
pub const SPACE_XS: Pixels = px(4.0);

/// Default gap between sibling elements in a row or column.
pub const SPACE_SM: Pixels = px(8.0);

/// Related but distinct: the gaps inside a group of controls.
pub const SPACE_MD: Pixels = px(12.0);

/// Between the rows of a settings page; also the content area's own padding.
pub const SPACE_LG: Pixels = px(16.0);

/// Between groups of settings within a page.
pub const SPACE_XL: Pixels = px(24.0);

/// Between major page regions.
pub const SPACE_2XL: Pixels = px(32.0);

// ------------------------------------------------------------------- icons --

/// Inline with text: a trailing chevron, a per-row action.
pub const ICON_SM: Pixels = px(14.0);

/// Standard icon: toolbars, list rows, buttons.
pub const ICON_MD: Pixels = px(16.0);

/// Primary affordance: transport controls, section headers.
pub const ICON_LG: Pixels = px(20.0);
