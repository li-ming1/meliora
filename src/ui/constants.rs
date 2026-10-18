use gpui::px;

//
// 尺寸
//
pub const APP_SHADOW_SIZE: gpui::Pixels = px(10.0);
pub const COLLAPSED_SIDEBAR_WIDTH: gpui::Pixels = px(52.0);

//
// 标题栏
//
/// 施加在 `WindowHeader` 左容器上的内边距。主窗口的品牌块用负外边距镜像这几个
/// 值，好让侧栏贴着窗口边缘对齐。
pub const TITLEBAR_LEFT_PAD_X: gpui::Pixels = px(12.0);
pub const TITLEBAR_LEFT_PAD_TOP: gpui::Pixels = px(7.0);
pub const TITLEBAR_LEFT_PAD_BOTTOM: gpui::Pixels = px(8.0);
