mod finder;

pub use finder::{ExtraItem, ExtraItemProvider, FinderItemLeft, PaletteItem};
use tracing::trace;

use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use nucleo::Utf32String;

use crate::ui::components::{
    input::{EnrichedInputAction, TextInput},
    palette::finder::Finder,
};
use crate::ui::theme::Theme;

/// 拆成两段的条目列表更新：稳定段（作为一份 `Arc` 共享，只在真正变化时重建——例如库检索索引）
/// 与易变尾段（每次更新整体替换——例如在线搜索结果）。稳定集很大的发射方可只刷新尾段而不碰
/// 稳定条目；finder 收到的仍是一份"稳定在前、易变在后"的平铺列表（见 `Finder::set_items`）。
pub struct SplitItems<T> {
    pub stable: Arc<Vec<Arc<T>>>,
    pub volatile: Vec<Arc<T>>,
}

// 手写 impl：克隆只自增 `Arc` 计数，不需要 `T: Clone` 约束。
impl<T> Clone for SplitItems<T> {
    fn clone(&self) -> Self {
        SplitItems {
            stable: self.stable.clone(),
            volatile: self.volatile.clone(),
        }
    }
}

impl<T> SplitItems<T> {
    /// 摊平成 finder 消费的那一份"稳定在前、易变在后"的列表。
    fn to_flat(&self) -> Vec<Arc<T>> {
        self.stable
            .iter()
            .chain(self.volatile.iter())
            .cloned()
            .collect()
    }
}

pub struct Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    input: Entity<TextInput>,
    handle: FocusHandle,
    finder: Option<Entity<Finder<T, MatcherFunc, OnAccept>>>,
    items: SplitItems<T>,
    extra_providers: Vec<ExtraItemProvider>,
}

impl<T, MatcherFunc, OnAccept> Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    pub fn new(
        cx: &mut App,
        items: Vec<Arc<T>>,
        matcher: MatcherFunc,
        on_accept: OnAccept,
        show: &Entity<bool>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let handle = cx.focus_handle();
            let weak_self = cx.weak_entity();

            let handler = move |action, _: &mut Window, cx: &mut App| {
                _ = weak_self.update(cx, |this: &mut Self, cx| {
                    if let Some(finder) = &this.finder {
                        cx.update_entity(finder, |_, cx| {
                            cx.emit(action);
                        });
                    }
                });
            };

            let input = TextInput::new(cx, handle.clone(), None, None, Some(Box::new(handler)));

            // 把输入变更接到 finder
            cx.subscribe(&input, move |this: &mut Self, _, ev: &SharedString, cx| {
                // 同时把查询透出给外部订阅者（酷狗搜索集成用它去拉在线结果）
                cx.emit(ev.clone());

                if let Some(finder) = &this.finder {
                    cx.update_entity(finder, |_, cx| {
                        cx.emit(ev.clone());
                    });
                }
            })
            .detach();

            // 把条目列表更新转发给 finder。平铺列表（其它所有 palette）存成"全部稳定"的拆分。
            cx.subscribe(
                &cx.entity(),
                move |this: &mut Self, _, items: &Vec<Arc<T>>, cx| {
                    this.items = SplitItems {
                        stable: Arc::new(items.clone()),
                        volatile: Vec::new(),
                    };

                    if let Some(finder) = &this.finder {
                        cx.update_entity(finder, |_, cx| {
                            cx.emit(items.clone());
                        });
                    }
                },
            )
            .detach();

            // 把拆分后的条目更新转发给 finder：finder 消费一份平铺的"稳定在前、易变在后"列表，
            // 所以稳定段在这里落地——它的逐项克隆只发生在这个边界上，仅易变段变化时上游不会
            // 重新克隆它。
            cx.subscribe(
                &cx.entity(),
                move |this: &mut Self, _, items: &SplitItems<T>, cx| {
                    this.items = items.clone();

                    if let Some(finder) = &this.finder {
                        cx.update_entity(finder, |_, cx| {
                            cx.emit(items.to_flat());
                        });
                    }
                },
            )
            .detach();

            let matcher = Arc::new(matcher);
            let on_accept = Arc::new(on_accept);

            cx.observe(show, move |this, show, cx| {
                if *show.read(cx) {
                    trace!("Creating finder for palette");

                    let finder =
                        Finder::new(cx, this.items.to_flat(), matcher.clone(), on_accept.clone());

                    let providers = this.extra_providers.clone();

                    finder.update(cx, |finder, cx| {
                        for provider in providers {
                            finder.register_extra_provider(provider, cx);
                        }

                        cx.notify();
                    });

                    this.finder = Some(finder);
                } else {
                    trace!("Destroying finder for palette");
                    this.finder = None;
                }

                cx.notify()
            })
            .detach();

            Palette {
                input,
                handle,
                finder: None,
                items: SplitItems {
                    stable: Arc::new(items),
                    volatile: Vec::new(),
                },
                extra_providers: Vec::new(),
            }
        })
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.handle.focus(window, cx);
    }

    pub fn reset(&self, cx: &mut Context<Self>) {
        cx.update_entity(&self.input, |input, cx| {
            input.reset();
            cx.notify();
        });
        if let Some(finder) = &self.finder {
            cx.update_entity(finder, |finder, cx| {
                finder.set_query("".to_string(), cx);
                finder.regenerate_list_state(cx);
                cx.notify();
            });
        }
    }

    pub fn register_extra_provider(&mut self, provider: ExtraItemProvider, cx: &mut Context<Self>) {
        let provider_clone = provider.clone();

        if let Some(finder) = &self.finder {
            cx.update_entity(finder, |finder, cx| {
                finder.register_extra_provider(provider_clone, cx);
                cx.notify();
            });
        }

        self.extra_providers.push(provider);
    }
}

impl<T, MatcherFunc, OnAccept> Render for Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        div()
            .w_full()
            .h_full()
            .overflow_hidden()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .p(px(12.0))
                    .line_height(px(14.0))
                    .h(px(40.0))
                    .text_sm()
                    .border_b(px(1.0))
                    .border_color(theme.elevated_border_color)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .h_full()
                    // FIXME: 古怪的布局问题，这里是个 hack；
                    // 最终这段应当被移除
                    .pb(px(40.0))
                    .when_some(self.finder.clone(), |this, finder| this.child(finder)),
            )
    }
}

impl<T, MatcherFunc, OnAccept> EventEmitter<Vec<Arc<T>>> for Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> EventEmitter<SplitItems<T>> for Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> EventEmitter<SharedString> for Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}

impl<T, MatcherFunc, OnAccept> EventEmitter<EnrichedInputAction>
    for Palette<T, MatcherFunc, OnAccept>
where
    T: Send + Sync + PartialEq + PaletteItem + 'static,
    MatcherFunc: Fn(&Arc<T>, &mut App) -> Utf32String + 'static,
    OnAccept: Fn(&Arc<T>, &mut App) + 'static,
{
}
