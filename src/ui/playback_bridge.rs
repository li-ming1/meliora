//! 播放线程事件通道的 UI 侧消费者：把播放事件镜像进 `Models` / `PlaybackInfo`
//! 实体的广播循环，外加图片回收漏斗拥有的帧间隔图集瓦片排空。逐字搬自
//! `playback::interface`（架构审计：playback 层不得 import `ui::models`）；
//! interface 本身保持为纯命令发送器。

use std::sync::Mutex;
use std::time::Duration;

use gpui::{App, AsyncApp, Entity};

use crate::library::Pool;
use crate::sync::RecoveredMutex;
use crate::{
    playback::{events::PlaybackEvent, interface::PlaybackInterface, thread::PlaybackState},
    power::PowerManager,
    ui::models::{CurrentTrack, ImageEvent, Models, PlaybackInfo},
};

/// 音量回声只有高于这条下限才更新 `prev_volume`：被记住的值必须在开关音量时
/// 仍然可见于 UI，即便用户把它一路拖到 `0`。
const MIN_REMEMBERED_VOLUME: f64 = 0.05;

/// [`start_broadcast`] 派生的空闲图集瓦片排空的节奏：让已逐出的瓦片在暂停期间
/// 也被回收（事件循环只在有事件到达时才排空）。
const IDLE_TILE_DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// 启动广播循环：它从播放线程读事件并据此更新数据模型。本函数只应调用一次，
/// 调用多于一次会 panic。
///
/// 这座桥借助 GPUI 的异步运行时来读消息而不阻塞渲染。事件一到就经 `recv().await`
/// 读出；繁忙帧留下的积压会归并为每种事件的最新一条，不过负载下执行器仍可能推迟
/// 唤醒。因此事件处理里不该做任何重操作，那些应当发给数据线程去做额外处理。
pub fn start_broadcast(interface: &mut PlaybackInterface, app: &mut App) {
    // 本函数的唯一职责就是从播放线程读事件并据此更新数据模型。
    let Some(mut events_rx) = interface.take_events_rx() else {
        panic!("broadcast thread already started");
    };

    let metadata_model = app.global::<Models>().metadata.clone();
    let albumart_model = app.global::<Models>().albumart.clone();
    let queue_model = app.global::<Models>().queue.clone();

    let playback_info = app.global::<PlaybackInfo>().clone();
    let power_manager = app.global::<PowerManager>().clone();
    let stats = app.global::<crate::stats::StatsHandle>().0.clone();
    let stats_pool = app.global::<Pool>().0.clone();

    app.spawn(async move |cx| {
        // `recv()` 返回 None 意味着播放线程已丢弃它的发送端；循环必须就此结束，
        // 否则会在 None 上空转。
        while let Some(event) = events_rx.recv().await {
            // 归并积压：一次繁忙的帧之后通道里可能积着几十条过期位置 tick，
            // 重放它们只是把同一批实体写入再做一遍。每个变体都是值替换语义
            // （最新者胜；QueueUpdated 是裸信号），所以每个变体只保留最后一次
            // 出现即可保持正确，其余全部丢弃。
            let mut latest = vec![event];
            while let Ok(next) = events_rx.try_recv() {
                let disc = std::mem::discriminant(&next);
                latest.retain(|e| std::mem::discriminant(e) != disc);
                latest.push(next);
            }
            for event in latest {
                match event {
                    PlaybackEvent::MetadataUpdate(v) => {
                        metadata_model.write(cx, *v);
                    }
                    PlaybackEvent::AlbumArtUpdate(v) => {
                        albumart_model.update(cx, |m, cx| {
                            if let Some(v) = v {
                                cx.emit(ImageEvent(v))
                            } else {
                                *m = None;
                                cx.notify()
                            }
                        });
                    }
                    PlaybackEvent::StateChanged(v) => {
                        playback_info.playback_state.write(cx, v);

                        if v == PlaybackState::Stopped {
                            playback_info.current_track.update(cx, |m, cx| {
                                *m = None;
                                cx.notify()
                            });
                        }

                        power_manager.set_state(cx, v);

                        record_and_flush(&stats, stats_pool.clone(), |recorder| {
                            recorder.on_state(v);
                            recorder.take_pending()
                        });
                    }
                    PlaybackEvent::PositionChanged(v) => {
                        playback_info.position.update(cx, |m, cx| {
                            *m = v;
                            cx.notify()
                        });

                        record_and_flush(&stats, stats_pool.clone(), |recorder| {
                            recorder.on_position(v);
                            recorder.take_pending()
                        });
                    }
                    PlaybackEvent::DurationChanged(v) => {
                        playback_info.duration.write(cx, v);
                    }
                    PlaybackEvent::SongChanged(path) => {
                        let (track_key, meta) = crate::stats::song_info(&path, &queue_model, cx);
                        record_and_flush(&stats, stats_pool.clone(), |recorder| {
                            recorder.on_song_changed(track_key, meta);
                            recorder.take_pending()
                        });

                        playback_info.current_track.update(cx, |m, cx| {
                            *m = Some(CurrentTrack::new(path.clone()));
                            cx.notify()
                        });
                    }
                    PlaybackEvent::QueueUpdated => {
                        queue_model.update(cx, |m, cx| {
                            // 给队列面板观察者用的内容变化信号
                            // （models::Queue::content_epoch）：在这里自增——这里是
                            // 播放事件变成队列实体写入的唯一漏斗——于是面板可以
                            // 在只带 QueuePositionChanged 的通知（每次切歌）上跳过
                            // 它那趟 O(n) 视图/摘要重扫。不伴随 QueueUpdated 的
                            // 纯路径替换（queue_manager 的 replace_current_path /
                            // replace_next_path）过去也从不通知 UI，且不改任何 uid
                            // 与时长，所以这个 epoch 恰好只跟踪重扫过去响应的那批通知。
                            m.content_epoch = m.content_epoch.wrapping_add(1);
                            cx.notify();
                        });
                    }
                    PlaybackEvent::ShuffleToggled(v) => {
                        playback_info.shuffling.write(cx, v);
                    }
                    PlaybackEvent::VolumeChanged(v) => {
                        // 相等性守卫：拖动滑块期间，回声通知会让源头那个滑块
                        // 每 tick 重渲染一次
                        update_if_changed(&playback_info.volume, cx, v);

                        // `prev_volume` 绝不能跌到下限之下：它的值需要在开关音量时
                        // 仍然可见于 UI，即便用户把它拖到 `0`。
                        if v > MIN_REMEMBERED_VOLUME {
                            update_if_changed(&playback_info.prev_volume, cx, v);
                        }
                    }
                    PlaybackEvent::QueuePositionChanged(v) => queue_model.update(cx, |m, cx| {
                        if m.position != v {
                            m.position = v;
                            cx.notify();
                        }
                    }),
                    PlaybackEvent::RepeatChanged(v) => {
                        update_if_changed(&playback_info.repeating, cx, v);
                    }
                    PlaybackEvent::StopAfterCurrentChanged(v) => {
                        update_if_changed(&playback_info.stop_after_current, cx, v);
                    }
                    PlaybackEvent::SampleRateChanged(rate) => {
                        update_if_changed(&playback_info.sample_rate, cx, rate);
                    }
                }
            }

            // 回收已逐出的封面图集瓦片。这个循环无论在窗口可见性如何都跑在 UI
            // 线程上——最小化的窗口仍会收到播放事件，而帧（因而 request_layout
            // 排空）不再产生。没有这段的话，空闲播放会每次切歌漏一个图集页。
            // 排空只能在帧之间运行（绝不在 mid-paint 中间，见 managed_image.rs）。
            cx.update(crate::ui::components::managed_image::drain_pending_tile_drops);
        }
    })
    .detach();

    // 空闲图集瓦片回收：上面那个事件循环只在有事件到达时排空；这个计时器让它在
    // 播放暂停时继续运转。刻意用 gpui 计时器，而不是 tokio::time——gpui 执行器
    // 在这个线程上没有 tokio reactor（启动期 panic，2026-09-08）。
    app.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(IDLE_TILE_DRAIN_INTERVAL)
                .await;
            cx.update(crate::ui::components::managed_image::drain_pending_tile_drops);
        }
    })
    .detach();
}

/// 把一次收听记录步进喂给共享的统计记录器并刷出它产出的行。毒化的锁会被从中
/// 恢复：更早一次记录步进里的 panic 绝不能把广播循环卡死。
fn record_and_flush<T>(
    stats: &Mutex<T>,
    pool: sqlx::SqlitePool,
    record: impl FnOnce(&mut T) -> Vec<crate::stats::ListenRow>,
) {
    let rows = record(&mut stats.lock_recovered());
    crate::stats::flush_rows_async(pool, rows);
}

/// 只在值真的不同时才把 `value` 写进实体并通知：拖动滑块期间的回声事件否则
/// 会让源控件每 tick 重渲染一次。
fn update_if_changed<T: PartialEq + 'static>(entity: &Entity<T>, cx: &mut AsyncApp, value: T) {
    entity.update(cx, |m, cx| {
        if *m != value {
            *m = value;
            cx.notify();
        }
    });
}
