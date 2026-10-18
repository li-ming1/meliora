//! 在线 provider 设置页（`kugou`）共用的基础设施：扫码登录流程（状态机、带失败
//! 上限的轮询循环、模态框）与下载目录输入框。与它的使用方一起受 feature 门控。

use std::{future::Future, sync::Arc, time::Duration};

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement, PathPromptOptions, RenderImage,
    SharedString, Styled, div, img, prelude::FluentBuilder, px,
};

use crate::{
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::components::managed_image::queue_orphan_tile_drop,
    ui::{
        components::{
            button::{ButtonIntent, button},
            modal::modal,
            textbox::Textbox,
        },
        theme::Theme,
    },
};

/// 轮询循环在放弃（按码失效处理）之前能容忍的连续传输失败次数——网络断了不能让
/// 循环空转到永远。
const QR_MAX_POLL_FAILURES: u32 = 10;

/// 模态框内展示的扫码登录进度。
#[derive(Clone)]
pub(crate) struct QrLoginState {
    image: Option<Arc<RenderImage>>,
    phase: QrPhase,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum QrPhase {
    Generating,
    Waiting,
    Scanned,
    Expired,
}

/// 归一化后的登录状态轮询结果；provider 在返回 `Success` 之前先持久化自己的会话。
pub(crate) enum QrPoll {
    Waiting,
    Scanned,
    Expired,
    Success,
}

/// 共享的扫码登录状态：模态框内容，外加一个代号计数——关闭模态框或重新生成时，
/// 它会让正在跑的轮询循环失效。
pub(crate) struct QrLogin {
    state: Option<QrLoginState>,
    generation: u64,
}

impl QrLogin {
    pub(crate) const fn new() -> Self {
        Self {
            state: None,
            generation: 0,
        }
    }

    /// 把 `state` 里当前的二维码图推进瓦片回收漏斗并清空状态。
    /// `ImageSource::Render` 的瓦片只有显式 `drop_image` 才会释放，所以每一个丢弃
    /// 二维码图的位置都必须走这里。
    fn recycle_current(&mut self) {
        if let Some(state) = self.state.take()
            && let Some(image) = state.image
        {
            queue_orphan_tile_drop(image);
        }
    }

    /// 为一次全新登录武装模态框；返回新的代号。
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        // "重新生成"会直接替换旧 state：旧二维码若已上传进图集，这里的
        // plain-drop 会泄漏瓦片——与 close() 一样推入回收漏斗。
        self.recycle_current();
        self.state = Some(QrLoginState {
            image: None,
            phase: QrPhase::Generating,
        });
        self.generation
    }

    /// 关闭模态框并让任何在跑的流程失效。
    pub(crate) fn close(&mut self) {
        self.generation += 1;
        // `ImageSource::Render` 的图不经过任何缓存，瓦片只有显式
        // drop_image 才会回收——把还挂着的二维码推入回收漏斗。
        self.recycle_current();
    }

    pub(crate) fn state(&self) -> Option<&QrLoginState> {
        self.state.as_ref()
    }

    /// `generation` 与在跑的流程不再一致时为 true。
    fn stale(&self, generation: u64) -> bool {
        self.generation != generation
    }

    /// 设置模态框阶段；返回值是否真的变了。
    fn set_phase(&mut self, phase: QrPhase) -> bool {
        if let Some(state) = &mut self.state
            && state.phase != phase
        {
            state.phase = phase;
            return true;
        }
        false
    }
}

impl Drop for QrLogin {
    fn drop(&mut self) {
        // begin()/close() 覆盖了模态内的所有替换点，但页面实体被丢弃时
        // （侧栏切换分节、设置窗口关闭）没有任何 close() 回调：还挂在
        // state 上的二维码瓦片会随实体静默消失。`ImageSource::Render`
        // 的瓦片只有显式 drop_image 才回收，所以这里必须走同一漏斗。
        self.recycle_current();
    }
}

/// 共享扫码登录流程的 provider 钩子，由在线设置页实现。网络调用跑在 Tokio runtime
/// 上；错误被压平成字符串，因为驱动方只会把它记日志或做 toast。
pub(crate) trait QrLoginHost: 'static + Sized {
    /// 登录状态轮询之间的间隔。
    const POLL_INTERVAL: Duration;

    /// 申请一个新的登录 key。
    fn create_key() -> impl Future<Output = Result<String, String>> + Send;
    /// 把 key 渲染成二维码图（跑在阻塞池）。
    fn build_qr(key: &str) -> Result<Arc<RenderImage>, String>;
    /// 轮询 `key` 的登录状态。
    fn poll(key: String) -> impl Future<Output = Result<QrPoll, String>> + Send;
    /// 请求失败时展示的 toast 文案。
    fn failed_toast(err: String) -> Toast;
    /// 当前扫码阶段的文案。
    fn phase_label(phase: QrPhase) -> SharedString;
    /// 登录成功之后的 UI 跟进动作；会话已经持久化，模态框由驱动方自己关闭。
    fn on_login(&mut self, cx: &mut Context<Self>);

    /// 该页持有的扫码状态。
    fn qr_login(&mut self) -> &mut QrLogin;

    /// 入口：打开模态框并启动流程。
    fn start_login(&mut self, cx: &mut Context<Self>) {
        spawn_login::<Self>(cx);
    }

    /// 关闭模态框。
    fn close_qr(&mut self, cx: &mut Context<Self>) {
        self.qr_login().close();
        cx.notify();
    }
}

/// 等待一个 Tokio 任务，并把 runtime 的 `Result` 外层压平成 provider 的
/// `Result<T, String>`：内层错误本来就是字符串，而被取消或 panic 的任务退化成
/// `JoinError` 的消息。
async fn flatten_task_result<T>(
    task: tokio::task::JoinHandle<Result<T, String>>,
) -> Result<T, String> {
    match task.await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(err.to_string()),
    }
}

/// 驱动一次扫码登录：申请 key、渲染图片，然后轮询直到成功、失效或撞上失败上限。
/// provider 特有的部分由 `H` 提供。
fn spawn_login<H: QrLoginHost>(cx: &mut Context<H>) {
    cx.spawn(async move |this, cx| {
        let generation = match this.update(cx, |this, _| this.qr_login().begin()) {
            Ok(generation) => generation,
            Err(_) => return, // 启动期间页面已被释放
        };

        // 第 1 步：申请一个新 key 并渲染成二维码图
        let key_task = crate::RUNTIME.spawn(async move { H::create_key().await });
        let key = match flatten_task_result(key_task).await {
            Ok(key) => key,
            Err(err) => {
                emit_toast(H::failed_toast(err));
                this.update(cx, |this, cx| {
                    if !this.qr_login().stale(generation) {
                        this.qr_login().close();
                        cx.notify();
                    }
                })
                .ok();
                return;
            }
        };

        let key_for_qr = key.clone();
        let image_task = crate::RUNTIME.spawn_blocking(move || H::build_qr(&key_for_qr));
        let image = flatten_task_result(image_task).await;
        let start_polling = this
            .update(cx, |this, cx| {
                if this.qr_login().stale(generation) {
                    return false;
                }
                match image {
                    Ok(image) => {
                        if let Some(state) = &mut this.qr_login().state {
                            state.image = Some(image);
                            state.phase = QrPhase::Waiting;
                        }
                        cx.notify();
                        true
                    }
                    Err(err) => {
                        tracing::error!("failed to render login qr: {err}");
                        emit_toast(H::failed_toast(err));
                        this.qr_login().close();
                        cx.notify();
                        false
                    }
                }
            })
            .unwrap_or(false);
        if !start_polling {
            return;
        }

        // 第 2 步：轮询直到手机端确认（或码失效）；反复的传输失败会按码失效一样
        // 放弃，网络断了不能让这个循环空转到永远。
        let mut failures = 0u32;
        loop {
            cx.background_executor().timer(H::POLL_INTERVAL).await;

            let poll_key = key.clone();
            let poll_task = crate::RUNTIME.spawn(async move { H::poll(poll_key).await });
            let check = flatten_task_result(poll_task).await;

            let keep_going = this
                .update(cx, |this, cx| {
                    if this.qr_login().stale(generation) || this.qr_login().state.is_none() {
                        return false;
                    }

                    match check {
                        Ok(QrPoll::Waiting) => {
                            failures = 0;
                            if this.qr_login().set_phase(QrPhase::Waiting) {
                                cx.notify();
                            }
                        }
                        Ok(QrPoll::Scanned) => {
                            failures = 0;
                            if this.qr_login().set_phase(QrPhase::Scanned) {
                                cx.notify();
                            }
                        }
                        Ok(QrPoll::Expired) => {
                            this.qr_login().set_phase(QrPhase::Expired);
                            cx.notify();
                            return false;
                        }
                        Ok(QrPoll::Success) => {
                            this.qr_login().close();
                            this.on_login(cx);
                            cx.notify();
                            return false;
                        }
                        Err(err) => {
                            tracing::warn!("qr login poll failed: {err}");
                            failures += 1;
                        }
                    }

                    if failures >= QR_MAX_POLL_FAILURES {
                        tracing::warn!(
                            "qr login poll gave up after {failures} consecutive failures"
                        );
                        this.qr_login().set_phase(QrPhase::Expired);
                        cx.notify();
                        return false;
                    }

                    true
                })
                .unwrap_or(false);

            if !keep_going {
                break;
            }
        }
    })
    .detach();
}

/// 渲染扫码登录模态框。`title`、`hint` 与按钮文案由页面提供，各自保留自己的 i18n
/// 键。
pub(crate) fn render_qr_modal<H: QrLoginHost>(
    qr: &QrLoginState,
    title: SharedString,
    hint: Option<SharedString>,
    regenerate_label: SharedString,
    cancel_label: SharedString,
    theme: &Theme,
    cx: &mut Context<H>,
) -> impl IntoElement {
    let weak = cx.weak_entity();

    modal()
        .on_exit(move |_, cx| {
            weak.update(cx, |this, cx| this.close_qr(cx)).ok();
        })
        .child(
            div()
                .w(px(380.0))
                .max_w_full()
                .p(px(24.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(px(18.0))
                        .font_weight(FontWeight::BOLD)
                        .child(title),
                )
                .children(
                    hint.map(|hint| div().text_sm().text_color(theme.text_secondary).child(hint)),
                )
                .child(match &qr.image {
                    Some(image) => div().rounded(px(theme.radius_md)).overflow_hidden().child(
                        img(image.clone())
                            .w(px(260.0))
                            .h(px(260.0))
                            .flex_shrink(0.0),
                    ),
                    None => div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(260.0))
                        .h(px(260.0))
                        .text_sm()
                        .text_color(theme.text_secondary)
                        // 与 Generating 阶段标签同文案。
                        .child(H::phase_label(QrPhase::Generating)),
                })
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.text_secondary)
                        .child(H::phase_label(qr.phase)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .when(qr.phase == QrPhase::Expired, |this| {
                            this.child(
                                button()
                                    .id("online-qr-regenerate")
                                    .intent(ButtonIntent::Primary)
                                    .child(regenerate_label)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.start_login(cx);
                                    })),
                            )
                        })
                        .child(
                            button()
                                .id("online-qr-cancel")
                                .child(cancel_label)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_qr(cx);
                                })),
                        ),
                ),
        )
}

/// 共用的下载目录输入框：惰性创建的 textbox 加原生文件夹选择器。经
/// `update_playback_settings` 持久化，因此两个 provider 写的是同一个
/// `playback.download_dir` 设置。
pub(crate) struct DownloadDirField {
    input: Option<Entity<Textbox>>,
}

impl DownloadDirField {
    pub(crate) const fn new() -> Self {
        Self { input: None }
    }

    /// 惰性创建的下载目录 textbox。提交空值即重置为平台默认的 Downloads 文件夹。
    pub(crate) fn input(&mut self, cx: &mut App) -> Entity<Textbox> {
        self.input
            .get_or_insert_with(|| {
                let current = cx
                    .global::<SettingsGlobal>()
                    .model
                    .read(cx)
                    .playback
                    .effective_download_dir()
                    .display()
                    .to_string();
                let input = Textbox::new_with_value_submit(cx, Default::default(), |value, cx| {
                    let trimmed = value.trim().to_string();
                    set_download_dir(cx, (!trimmed.is_empty()).then_some(trimmed));
                });
                input.update(cx, |textbox, cx| {
                    textbox.set_value(cx, SharedString::from(current));
                });
                input
            })
            .clone()
    }

    /// 打开原生文件夹选择器；选中的文件夹写进 textbox 并立刻保存。
    pub(crate) fn browse(&self, prompt: SharedString, cx: &mut App) {
        let path_future = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt),
        });
        let input = self.input.clone();
        cx.spawn(async move |cx| {
            let Ok(Ok(Some(paths))) = path_future.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let dir = dir.display().to_string();
            if let Some(input) = input {
                input.update(cx, |textbox, cx| {
                    textbox.set_value(cx, SharedString::from(dir.clone()));
                });
            }
            cx.update(|cx| set_download_dir(cx, Some(dir)));
        })
        .detach();
    }
}

/// 持久化下载目录设置（空/None = 平台默认）。
fn set_download_dir(cx: &mut App, dir: Option<String>) {
    let settings = cx.global::<SettingsGlobal>().model.clone();
    super::update_playback_settings(&settings, cx, move |playback| {
        playback.download_dir = dir;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_login_generation_invalidates_stale_flows() {
        let mut qr = QrLogin::new();
        assert!(qr.state().is_none());

        let g1 = qr.begin();
        assert!(!qr.stale(g1));
        qr.set_phase(QrPhase::Scanned);
        assert_eq!(qr.state().unwrap().phase, QrPhase::Scanned);

        // 重新生成会武装一个新代号并让旧代号失效。
        let g2 = qr.begin();
        assert_ne!(g1, g2);
        assert!(qr.stale(g1));

        // 关闭（取消/登出）会让全部在途流程失效并清空模态框。
        qr.close();
        assert!(qr.state().is_none());
        assert!(qr.stale(g2));
    }
}
