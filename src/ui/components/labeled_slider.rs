use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, Div, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StyleRefinement, Styled, Window, div, px, relative,
};

use crate::ui::{components::slider::slider, theme::Theme};

type ChangeHandler = dyn FnMut(f32, &mut Window, &mut App);
type ValueFormatter = dyn Fn(f32) -> SharedString;

#[derive(IntoElement)]
pub struct LabeledSlider {
    id: ElementId,
    slider_id: Option<ElementId>,
    min: f32,
    max: f32,
    value: f32,
    default_value: Option<f32>,
    on_change: Option<Rc<RefCell<ChangeHandler>>>,
    formatter: Rc<ValueFormatter>,
    /// 端点标签缓存：由 min/max/format_value setter 一次性预格式化。
    min_label: Option<SharedString>,
    max_label: Option<SharedString>,
    div: Div,
}

impl LabeledSlider {
    pub fn slider_id(mut self, id: impl Into<ElementId>) -> Self {
        self.slider_id = Some(id.into());
        self
    }

    pub fn min(mut self, min: f32) -> Self {
        self.min = min;
        self.recompute_static_labels();
        self
    }

    pub fn max(mut self, max: f32) -> Self {
        self.max = max;
        self.recompute_static_labels();
        self
    }

    pub fn value(mut self, value: f32) -> Self {
        self.value = value;
        self
    }

    pub fn default_value(mut self, value: f32) -> Self {
        self.default_value = Some(value);
        self
    }

    pub fn on_change(
        mut self,
        on_change: impl FnMut(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(RefCell::new(on_change)));
        self
    }

    pub fn format_value(mut self, formatter: impl Fn(f32) -> SharedString + 'static) -> Self {
        self.formatter = Rc::new(formatter);
        self.recompute_static_labels();
        self
    }

    /// 端点标签只依赖 min/max/formatter：按 low/high 交换规则预格式化并失效
    /// 重算，拖动期 render 不再逐帧对不变的端点做 format! 堆分配。
    fn recompute_static_labels(&mut self) {
        let low = self.min.min(self.max);
        let high = self.min.max(self.max);
        self.min_label = Some((self.formatter)(low));
        self.max_label = Some((self.formatter)(high));
    }
}

impl Styled for LabeledSlider {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for LabeledSlider {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let low = self.min.min(self.max);
        let high = self.min.max(self.max);
        let clamped = self.value.clamp(low, high);
        let range = (high - low).max(f32::EPSILON);
        let normalized = ((clamped - low) / range).clamp(0.0, 1.0);
        let left_flex = normalized.max(0.0);
        let right_flex = (1.0 - normalized).max(0.0);

        let formatter = self.formatter.clone();
        let min_text = self.min_label.clone().unwrap_or_else(|| (formatter)(low));
        let max_text = self.max_label.clone().unwrap_or_else(|| (formatter)(high));
        let current_text = (formatter)(clamped);

        // Maps a 0..1 track position back onto the [low, high] value range.
        let denormalize = move |t: f32| (low + (high - low) * t).clamp(low, high);

        let on_change = self.on_change.clone();
        let default_value = self.default_value;
        let slider_id = self
            .slider_id
            .clone()
            .unwrap_or_else(|| "labeled-slider-track".into());
        let track = slider()
            .id(slider_id)
            .w_full()
            .h(px(8.0))
            .rounded(px(theme.radius_sm))
            .value(normalized);
        let slider = if let Some(on_change) = on_change {
            let on_change_click = on_change.clone();
            track
                .on_change(move |v, window, cx| {
                    (on_change_click.borrow_mut())(denormalize(v), window, cx);
                })
                .on_double_click(move |window, cx| {
                    let fallback = denormalize(0.5);
                    let reset_value = default_value.unwrap_or(fallback).clamp(low, high);
                    (on_change.borrow_mut())(reset_value, window, cx);
                })
        } else {
            track
        };

        self.div
            .id(self.id)
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(26.0))
                    .text_xs()
                    .text_color(theme.text_secondary)
                    .child(div().absolute().left(px(0.0)).top(px(4.0)).child(min_text))
                    .child(div().absolute().right(px(0.0)).top(px(4.0)).child(max_text))
                    .child(
                        div()
                            .absolute()
                            .left(px(0.0))
                            .right(px(0.0))
                            .top(px(0.0))
                            .flex()
                            .items_center()
                            .child(div().h(px(1.0)).w(relative(left_flex)))
                            .child(
                                div()
                                    .text_color(theme.text)
                                    .border_1()
                                    .border_color(theme.elevated_border_color)
                                    .bg(theme.elevated_background)
                                    .rounded(px(theme.radius_sm))
                                    .px(px(6.0))
                                    .py(px(1.0))
                                    .child(current_text),
                            )
                            .child(div().h(px(1.0)).w(relative(right_flex))),
                    ),
            )
            .child(slider)
    }
}

pub fn labeled_slider(id: impl Into<ElementId>) -> LabeledSlider {
    LabeledSlider {
        id: id.into(),
        slider_id: None,
        min: 0.0,
        max: 1.0,
        value: 0.0,
        default_value: None,
        on_change: None,
        formatter: Rc::new(|value| format!("{value:.2}").into()),
        min_label: None,
        max_label: None,
        div: div(),
    }
}
