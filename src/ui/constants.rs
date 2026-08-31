use gpui::px;

//
// SIZES
//
pub const APP_SHADOW_SIZE: gpui::Pixels = px(10.0);
pub const COLLAPSED_SIDEBAR_WIDTH: gpui::Pixels = px(52.0);

//
// TITLEBAR
//
/// Padding applied to `WindowHeader`'s left container. The main window's
/// brand block mirrors these with negative margins so the sidebar runs flush
/// to the window edge.
pub const TITLEBAR_LEFT_PAD_X: gpui::Pixels = px(12.0);
pub const TITLEBAR_LEFT_PAD_TOP: gpui::Pixels = px(7.0);
pub const TITLEBAR_LEFT_PAD_BOTTOM: gpui::Pixels = px(8.0);
