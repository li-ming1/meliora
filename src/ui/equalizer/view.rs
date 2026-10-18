use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::ui::design::ICON_SM;
use cntp_i18n::tr;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Pixels, Point, Render, StatefulInteractiveElement,
    Styled, Task, Window, actions, div, prelude::FluentBuilder, px, rgb,
};

use crate::{
    playback::interface::PlaybackInterface,
    settings::{
        Settings, SettingsGlobal,
        equalizer::{EqBandSettings, EqualizerSettings, MAX_EQ_BANDS},
        save_settings,
    },
    ui::{
        components::{
            icons::{ALERT_CIRCLE, icon},
            tooltip::build_tooltip,
        },
        equalizer::{
            band_editor::{BandEdit, band_editor},
            graph::{PlotSlot, dot_position, eq_graph},
            mapping::{nudge_frequency, nudge_gain},
            spectrum::{SpectrumData, SpectrumState, ensure_analyzer},
        },
        global_actions::CloseWindow,
        models::PlaybackInfo,
        theme::Theme,
    },
};

actions!(eq, [Dismiss]);

/// 冻结的浮层位置，外加能在变化时把它重新锚定的那几个输入。
#[derive(Clone, Copy)]
struct PopoverAnchor {
    selected: Option<usize>,
    plot: Option<Bounds<Pixels>>,
    point: Point<Pixels>,
}

/// 均衡器设置的工作副本：条数上限为 MAX_EQ_BANDS 并做过清洗。
fn synced_config(settings: &Settings) -> EqualizerSettings {
    let mut config = settings.playback.equalizer.clone();
    config.bands.truncate(MAX_EQ_BANDS);
    config.sanitize();
    config
}

pub struct EqualizerView {
    settings: Entity<Settings>,
    config: EqualizerSettings,
    sample_rate: u32,
    selected: Option<usize>,
    dragging: bool,
    popover_anchor: Option<PopoverAnchor>,
    plot_slot: PlotSlot,
    spectrum: Option<Entity<SpectrumData>>,
    /// 门控音频抽头的共享查看者计数，视图释放时减一。
    spectrum_viewers: Option<Arc<AtomicUsize>>,
    /// 第一次点击头部的重置按钮只是把它武装起来，第二次才清空频段。
    reset_armed: bool,
    /// 如果那第二次确认点击迟迟不来，就解除武装。
    reset_disarm_task: Option<Task<()>>,
    clip_latched: bool,
    save_task: Option<Task<()>>,
    /// 拖拽独占状态期间被挡住的重新载入，拖拽结束时应用。
    pending_reload: Option<EqualizerSettings>,
    focus_handle: FocusHandle,
    /// 选中项变化应当把键盘焦点拉到本视图时置位。
    focus_pending: bool,
}

impl EqualizerView {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let settings = cx.global::<SettingsGlobal>().model.clone();
            let config = synced_config(settings.read(cx));

            // 热重载与外部编辑会同步进来，拖拽独占状态期间被挡住
            cx.observe(&settings, |this: &mut Self, settings, cx| {
                let config = synced_config(settings.read(cx));
                if this.dragging {
                    this.pending_reload = Some(config);
                    return;
                }
                if config != this.config {
                    this.config = config;
                    this.drop_stale_selection();
                    cx.notify();
                }
            })
            .detach();

            // 待处理的防抖保存会随视图一起消亡，释放时把它刷出去
            cx.on_release(|this, cx| {
                // 少一个打开的视图，最后一个视图离开后抽头门就关上
                if let Some(viewers) = &this.spectrum_viewers {
                    viewers.fetch_sub(1, Ordering::Relaxed);
                }
                // 同类相比：工作副本的条数上限是 MAX_EQ_BANDS
                let mut stored = this.settings.read(cx).playback.equalizer.clone();
                stored.bands.truncate(MAX_EQ_BANDS);
                if this.save_task.is_none() && stored == this.config {
                    return;
                }
                let config = this.config.clone();
                this.settings.update(cx, |settings, cx| {
                    settings.playback.equalizer = config;
                    save_settings(cx, settings);
                    cx.notify();
                });
            })
            .detach();

            // 输出流的采样率变化时重绘曲线
            let sample_rate = if cx.has_global::<PlaybackInfo>() {
                let model = cx.global::<PlaybackInfo>().sample_rate.clone();
                let rate = *model.read(cx);
                cx.observe(&model, |this: &mut Self, model, cx| {
                    let rate = *model.read(cx);
                    if rate != this.sample_rate {
                        this.sample_rate = rate;
                        cx.notify();
                    }
                })
                .detach();
                rate
            } else {
                0
            };

            // 与进程同寿的分析器发布进一个共享实体，打开的视图为抽头把关
            ensure_analyzer(cx);
            let (spectrum, spectrum_viewers) = if cx.has_global::<SpectrumState>() {
                let (data, viewers) = {
                    let state = cx.global::<SpectrumState>();
                    (state.data.clone(), state.viewers.clone())
                };
                viewers.fetch_add(1, Ordering::Relaxed);
                cx.observe(&data, |_, _, cx| cx.notify()).detach();
                (Some(data), Some(viewers))
            } else {
                (None, None)
            };

            Self {
                settings,
                config,
                sample_rate,
                selected: None,
                dragging: false,
                popover_anchor: None,
                plot_slot: Rc::new(Cell::new(None)),
                spectrum,
                spectrum_viewers,
                reset_armed: false,
                reset_disarm_task: None,
                clip_latched: false,
                save_task: None,
                pending_reload: None,
                focus_handle: cx.focus_handle(),
                focus_pending: false,
            }
        })
    }

    /// 移除一个频段并保持选中项自洽。
    fn remove_band(&mut self, cx: &mut Context<Self>, index: usize) {
        self.mutate(cx, |config| {
            if index < config.bands.len() {
                config.bands.remove(index);
            }
        });
        match self.selected {
            Some(selected) if selected == index => self.selected = None,
            Some(selected) if selected > index => self.selected = Some(selected - 1),
            _ => {}
        }
    }

    /// 选中项指向（变短后的）频段列表之外时把它丢掉。
    fn drop_stale_selection(&mut self) {
        if self.selected.is_some_and(|i| i >= self.config.bands.len()) {
            self.selected = None;
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.mutate(cx, |config| config.enabled = enabled);
    }

    pub fn volume_compensation(&self) -> bool {
        self.config.volume_compensation
    }

    pub fn set_volume_compensation(&mut self, on: bool, cx: &mut Context<Self>) {
        self.mutate(cx, |config| config.volume_compensation = on);
    }

    pub fn request_reset(&mut self, cx: &mut Context<Self>) {
        if self.reset_armed {
            self.reset_armed = false;
            self.reset_disarm_task = None;
            self.selected = None;
            self.mutate(cx, |config| config.bands.clear());
        } else {
            self.reset_armed = true;
            // 武装之后很久才来的第二次点击不得清空频段
            self.reset_disarm_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                this.update(cx, |this, cx| {
                    this.reset_armed = false;
                    cx.notify();
                })
                .ok();
            }));
            cx.notify();
        }
    }

    pub fn reset_armed(&self) -> bool {
        self.reset_armed
    }

    /// 所有编辑的唯一汇聚点：先应用，再把结果推向所有下游。
    pub fn mutate(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut EqualizerSettings)) {
        update(&mut self.config);
        self.config.bands.truncate(MAX_EQ_BANDS);
        self.push_config(cx);
    }

    /// `mutate` 的单频段切片：索引越界即静默跳过——事件里带的索引可能在编辑到达
    /// 之前就已经过期（频段被删或清空）。四处"取 `get_mut` 再写字段"的手抄展开
    /// 此前各自重复这条守卫。
    fn mutate_band(
        &mut self,
        cx: &mut Context<Self>,
        index: usize,
        update: impl FnOnce(&mut EqBandSettings),
    ) {
        self.mutate(cx, |config| {
            if let Some(band) = config.bands.get_mut(index) {
                update(band);
            }
        });
    }

    /// 把工作副本推给 DSP 与设置模型，只对落盘做防抖。
    fn push_config(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<PlaybackInterface>() {
            cx.global::<PlaybackInterface>()
                .set_equalizer(self.config.clone());
        }

        // 模型始终跟随最新一次编辑，于是防抖期间的保存或重载拿到的都是当前值；
        // 观察者里的相等性守卫能防止这次 notify 回流到自己。
        // 这里刻意**不** notify：DSP 在上面已拿到配置，本视图在下面自己 notify，
        // 而对设置模型的 notify 会级联到应用级的 refresh_windows 观察者——拖拽时
        // 每次鼠标移动都重绘所有窗口。后沿保存直接读模型，settings.json 落盘时
        // 文件监视器会产生一次真正的通知。
        let config = self.config.clone();
        self.settings.update(cx, |settings, _| {
            settings.playback.equalizer = config;
        });

        // 在后沿落盘，于是密集编辑折叠成一次写入；闭包触发时保存模型当时的内容
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            this.update(cx, |this, cx| {
                // 清掉句柄，让释放守卫的"无待处理 && 无差异"快路径重新可用；
                // 更新的编辑会武装一个新任务（这个被丢弃时，其本体尚未运行就被取消）。
                this.save_task = None;
                this.settings
                    .update(cx, |settings, cx| save_settings(cx, settings));
            })
            .ok();
        }));

        cx.notify();
    }

    /// Escape 取消选中频段；没有选中时这个键继续下传给关闭窗口。
    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.take().is_some() {
            cx.notify();
        } else {
            cx.dispatch_action(&CloseWindow);
        }
    }

    /// 方向键微调选中的频段，Shift 缩小步长。
    fn nudge(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        let Some(band) = self.config.bands.get(index).copied() else {
            return;
        };
        let fine = ev.keystroke.modifiers.shift;
        let (frequency, gain_db) = match ev.keystroke.key.as_str() {
            "left" => (nudge_frequency(band.frequency, -1.0, fine), band.gain_db),
            "right" => (nudge_frequency(band.frequency, 1.0, fine), band.gain_db),
            "up" if band.kind.has_gain() => (band.frequency, nudge_gain(band.gain_db, 1.0, fine)),
            "down" if band.kind.has_gain() => {
                (band.frequency, nudge_gain(band.gain_db, -1.0, fine))
            }
            _ => return,
        };
        window.prevent_default();
        cx.stop_propagation();
        self.mutate_band(cx, index, |band| {
            band.frequency = frequency;
            band.gain_db = gain_db;
        });
    }
}

impl Render for EqualizerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Equalizer);
        // 只有本视图位于焦点路径上时键事件才会到达这里
        if self.focus_pending {
            self.focus_pending = false;
            if self.selected.is_some() {
                self.focus_handle.focus(window, cx);
            }
        }

        let view = cx.entity();

        let plot = self.plot_slot.get();
        let dot = self
            .selected
            .and_then(|index| Some(dot_position(self.config.bands.get(index)?, plot?)));
        // 锚点一经算出就冻结，在拖拽结束、选中变化或布局变化时释放
        let anchor = self
            .popover_anchor
            .filter(|anchor| anchor.selected == self.selected && anchor.plot == plot)
            .map(|anchor| anchor.point)
            .or(dot);
        self.popover_anchor = anchor.map(|point| PopoverAnchor {
            selected: self.selected,
            plot,
            point,
        });
        let editor = anchor.and_then(|point| {
            let index = self.selected?;
            let band = *self.config.bands.get(index)?;
            Some((index, point, band))
        });

        let (spectrum_pre, spectrum_post, clipping) = self
            .spectrum
            .as_ref()
            .map(|spectrum| {
                let spectrum = spectrum.read(cx);
                (
                    spectrum.pre.clone(),
                    spectrum.post.clone(),
                    spectrum.clipping,
                )
            })
            .unwrap_or_default();

        if clipping {
            self.clip_latched = true;
        }

        let mut graph = eq_graph("eq-graph", &self.config)
            .selected(self.selected)
            .spectrum(spectrum_pre, spectrum_post)
            .plot_slot(self.plot_slot.clone());
        if self.sample_rate > 0 {
            graph = graph.sample_rate(f64::from(self.sample_rate));
        }

        let graph = graph
            .on_band_change({
                let view = view.clone();
                move |index, frequency, gain_db, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_band(cx, index, |band| {
                            band.frequency = frequency;
                            if band.kind.has_gain() {
                                band.gain_db = gain_db;
                            }
                        });
                    });
                }
            })
            .on_select({
                let view = view.clone();
                move |selected, cx| {
                    view.update(cx, |this, cx| {
                        this.selected = selected;
                        this.focus_pending = selected.is_some();
                        cx.notify();
                    });
                }
            })
            .on_remove({
                let view = view.clone();
                move |index, cx| {
                    view.update(cx, |this, cx| this.remove_band(cx, index));
                }
            })
            .on_toggle_enabled({
                let view = view.clone();
                move |index, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_band(cx, index, |band| band.enabled = !band.enabled);
                    });
                }
            })
            .on_scroll_q({
                let view = view.clone();
                move |index, q, cx| {
                    view.update(cx, |this, cx| {
                        this.mutate_band(cx, index, |band| band.q = q);
                    });
                }
            })
            .on_add({
                let view = view.clone();
                move |frequency, gain_db, cx| {
                    view.update(cx, |this, cx| {
                        let band = EqBandSettings {
                            frequency,
                            gain_db,
                            ..Default::default()
                        };
                        this.mutate(cx, |config| config.bands.push(band));
                        let index = this.config.bands.len() - 1;
                        this.selected = Some(index);
                        this.focus_pending = true;
                        index
                    })
                }
            })
            .on_drag_active({
                move |active, cx| {
                    view.update(cx, |this, cx| {
                        this.dragging = active;
                        if !active {
                            // 把浮层重新锚定到圆点的静止位置
                            this.popover_anchor = None;
                            // 落实那次在拖拽独占状态期间被挡住的重新载入
                            if let Some(config) = this.pending_reload.take()
                                && config != this.config
                            {
                                this.config = config;
                                this.drop_stale_selection();
                                this.push_config(cx);
                            }
                        }
                    });
                }
            });

        div()
            .id("equalizer-view")
            .track_focus(&self.focus_handle)
            .role(gpui::Role::Group)
            .key_context("EqualizerView")
            .on_action(cx.listener(Self::dismiss))
            .on_key_down(cx.listener(Self::nudge))
            .flex_grow(1.0)
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .relative()
            .child(graph.flex_grow(1.0).min_h(px(160.0)))
            .when(self.clip_latched, |this| {
                let clip_bg = cx.global::<Theme>().status_error;
                this.child(
                    div()
                        .id("eq-clip-badge")
                        .absolute()
                        .top(px(10.0))
                        .right(px(10.0))
                        .occlude()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(8.0))
                        .py(px(2.0))
                        .rounded_full()
                        .bg(clip_bg)
                        .text_color(rgb(0xffffff))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clip_latched = false;
                            cx.notify();
                        }))
                        .tooltip(build_tooltip(tr!(
                            "EQ_CLIP_TOOLTIP",
                            "Output is clipping — lower band gains. Click to dismiss."
                        )))
                        .child(icon(ALERT_CIRCLE).my_auto().size(ICON_SM))
                        .child(
                            div()
                                .text_xs()
                                .line_height(px(16.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr!("EQ_CLIP", "Clip")),
                        ),
                )
            })
            .when_some(editor, |this, (index, point, band)| {
                let view = cx.entity();
                this.child(
                    band_editor(index, band, point)
                        .on_edit({
                            let view = view.clone();
                            move |edit, cx| {
                                view.update(cx, |this, cx| {
                                    this.mutate(cx, |config| {
                                        let Some(band) = config.bands.get_mut(index) else {
                                            return;
                                        };
                                        match edit {
                                            BandEdit::Kind(kind) => {
                                                // 切换类型时跟随默认 Q，自定义值则保留
                                                if band.q == band.kind.default_q() {
                                                    band.q = kind.default_q();
                                                }
                                                band.kind = kind;
                                            }
                                            BandEdit::Frequency(frequency) => {
                                                band.frequency = frequency
                                            }
                                            BandEdit::Gain(gain_db) => band.gain_db = gain_db,
                                            BandEdit::Q(q) => band.q = q,
                                            BandEdit::Enabled(enabled) => band.enabled = enabled,
                                        }
                                    });
                                });
                            }
                        })
                        .on_remove({
                            let view = view.clone();
                            move |cx| view.update(cx, |this, cx| this.remove_band(cx, index))
                        })
                        .on_dismiss(move |_, cx| {
                            view.update(cx, |this, cx| {
                                this.selected = None;
                                cx.notify();
                            });
                        }),
                )
            })
    }
}
