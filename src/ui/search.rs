pub mod model;
pub mod search_item;

use crate::ui::components::modal::ModalActive;

use gpui::*;
use model::SearchModel;

use super::{
    components::modal::modal,
    global_actions::Search,
    library::{NavigationHistory, ViewSwitchMessage},
    models::Models,
};

/// 搜索模态视图：承载库搜索面板，把它的导航事件转发给主视图切换器，
/// 且另一个模态框活动期间不会打开。
pub struct SearchView {
    show: Entity<bool>,
    search: Entity<SearchModel>,
    view_switcher: Entity<NavigationHistory>,
}

impl SearchView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let show = cx.new(|_| false);
            let show_clone = show.clone();
            let search = SearchModel::new(cx, &show);

            App::on_action(cx, move |_: &Search, cx| {
                if super::app::find_main_window(cx)
                    .is_some_and(|main| cx.global::<ModalActive>().active_in(main.window_id()))
                {
                    return;
                }

                // 先关掉艺人选择浮层，否则它会敞开着留在搜索模态框下方。
                super::artist_picker::close(cx);

                show_clone.update(cx, |m, cx| {
                    *m = true;
                    cx.notify();
                });
            });

            cx.subscribe(
                &search,
                |this: &mut SearchView, _, ev: &ViewSwitchMessage, cx| {
                    this.view_switcher.update(cx, |_, cx| {
                        cx.emit(*ev);
                    });
                    this.reset(cx);
                },
            )
            .detach();

            cx.observe(&show, |_, _, cx| {
                cx.notify();
            })
            .detach();

            SearchView {
                view_switcher: cx.global::<Models>().switcher_model.clone(),
                show,
                search,
            }
        })
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        cx.update_entity(&self.search, |search, cx| {
            search.reset(cx);
            cx.notify();
        });
        self.show.update(cx, |m, cx| {
            *m = false;
            cx.notify();
        })
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Search);
        let show = self.show.clone();
        let is_open = show.read(cx);
        let weak = cx.weak_entity();

        if *is_open {
            // 焦点给面板的输入框，而不是搜索视图本身。
            cx.update_entity(&self.search, |search, cx| {
                search.focus(window, cx);
            });

            modal()
                .on_exit(move |_, cx| {
                    // 模态框退出时这个视图实体可能已经没了（正常的销毁时序）——
                    // 记录一下继续走，不要中断。
                    if let Err(err) = weak.update(cx, |this, cx| {
                        this.reset(cx);
                    }) {
                        tracing::debug!(%err, "search view gone on modal exit");
                    }
                })
                .child(div().w(px(550.0)).h(px(500.0)).child(self.search.clone()))
                .into_any_element()
        } else {
            div().into_any_element()
        }
    }
}
