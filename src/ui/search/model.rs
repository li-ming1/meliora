use std::sync::Arc;
#[cfg(feature = "online_sources")]
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, EventEmitter, IntoElement, Render, Window};
// 订阅回调的 query 参数类型，只在 online_sources 的合并段订阅里出现。
#[cfg(feature = "online_sources")]
use gpui::SharedString;
use nucleo::Utf32String;
use tracing::debug;

use crate::{
    library::{db, scan::ScanEvent},
    ui::{
        app::Pool,
        availability::compute_available_albums,
        components::palette::{Palette, SplitItems},
        library::ViewSwitchMessage,
        models::Models,
    },
};

use super::search_item::SearchPaletteItem;

type MatcherFunc = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) -> Utf32String + 'static>;
type OnAccept = Box<dyn Fn(&Arc<SearchPaletteItem>, &mut App) + 'static>;

pub struct SearchModel {
    palette: Entity<Palette<SearchPaletteItem, MatcherFunc, OnAccept>>,
    /// 本地（音乐库）条目，保留下来以便把在线结果合并进去。整个 `Arc` 共享：
    /// 只涉及在线的更新会重发另一段而不碰这一段（音乐库量级的）内容。只有重建
    /// 成功时才整体交换 `Arc`（成功才交换）：扫描完成到新重建落地之间，旧索引保持
    /// 安装状态，本地那一段绝不会变黑。
    local_items: Arc<Vec<Arc<SearchPaletteItem>>>,
    /// 本地索引失效时递增；后台的在途载入会与之比对，过期的载入无法覆盖更新的那次。
    load_generation: u64,
    /// 是否存在（已完成的）本地索引。与 `local_items.is_empty()` 区分开，好让
    /// 面板打开的观察者不会在已有载入在途时每次打开都叠一个新载入。
    local_items_loaded: bool,
    #[cfg(feature = "kugou")]
    kugou_items: Vec<Arc<SearchPaletteItem>>,
    /// 每次查询变化都递增；在途的在线搜索与之比对，过期结果会被丢弃。
    #[cfg(feature = "kugou")]
    kugou_query_generation: u64,
}

/// 在后台 runtime 上载入全库搜索索引：三次全表查询外加每个不同曲目路径一次
/// `exists()` 统计，在 10 万条的库上要花好几秒，而它过去是在第一次打开面板的那一帧
/// 里同步跑的。
///
/// 三次核心表查询里的错误会让本次重建中止（返回 `Err`），好让调用方保留上一份索引
/// 而不是换上一个空索引；可用性只是逐专辑的装饰，它失败时仍退回空集合。
async fn load_search_items_off_thread(
    pool: sqlx::SqlitePool,
) -> Result<Vec<Arc<SearchPaletteItem>>, String> {
    let availability_rows = db::list_album_availability(&pool)
        .await
        .unwrap_or_else(|e| {
            debug!("Failed to load album availability for search: {:?}", e);
            Vec::new()
        });
    let available_albums =
        tokio::task::spawn_blocking(move || compute_available_albums(availability_rows))
            .await
            .unwrap_or_else(|e| {
                debug!("Failed to compute album availability for search: {:?}", e);
                Default::default()
            });

    let albums = db::list_albums_search(&pool)
        .await
        .map_err(|e| format!("albums query failed: {e:?}"))?
        .into_iter()
        .map(|(id, title, artist_override, artists)| {
            (
                id,
                title,
                artist_override,
                artists,
                available_albums.contains(&id),
            )
        })
        .collect();

    let artists = db::list_artists_search(&pool)
        .await
        .map_err(|e| format!("artists query failed: {e:?}"))?;

    let tracks = db::list_tracks_search(&pool)
        .await
        .map_err(|e| format!("tracks query failed: {e:?}"))?;

    Ok(SearchPaletteItem::from_search_results(
        albums, artists, tracks,
    ))
}

impl SearchModel {
    pub fn new(cx: &mut App, show: &Entity<bool>) -> Entity<SearchModel> {
        cx.new(|cx| {
            let weak_self = cx.weak_entity();

            // 搜索文本在构造时就为每个条目算好（见
            // `SearchPaletteItem::from_search_results` 与下面的在线查询处理），
            // 所以注入索引时永不做逐条目格式化。
            let matcher: MatcherFunc = Box::new(|item, _| {
                Utf32String::from(match item.as_ref() {
                    SearchPaletteItem::Album { search_text, .. }
                    | SearchPaletteItem::Artist { search_text, .. }
                    | SearchPaletteItem::Track { search_text, .. } => search_text.as_ref(),
                    #[cfg(feature = "kugou")]
                    SearchPaletteItem::KugouTrack { search_text, .. } => search_text.as_ref(),
                })
            });

            let on_accept: OnAccept = Box::new(move |item, cx| {
                let event = match item.as_ref() {
                    SearchPaletteItem::Album { id, .. } => {
                        Some(ViewSwitchMessage::Release(*id, None))
                    }
                    SearchPaletteItem::Artist { id, .. } => Some(ViewSwitchMessage::Artist(*id)),
                    SearchPaletteItem::Track { id, album_id, .. } => album_id
                        .as_ref()
                        .map(|album_id| ViewSwitchMessage::Release(*album_id, Some(*id))),
                    #[cfg(feature = "kugou")]
                    SearchPaletteItem::KugouTrack { track, .. } => {
                        crate::ui::kugou::play_track_now(cx, track);
                        None
                    }
                };

                let Some(event) = event else {
                    return;
                };

                if let Some(search_model) = weak_self.upgrade() {
                    search_model.update(cx, |_: &mut SearchModel, cx| {
                        cx.emit(event);
                    });
                }
            });

            // 不要在这里构建全库索引：SearchView 随主窗口一起创建，而三次全表查询
            // 加一轮可用性检查在大库上会把首帧阻塞住。第一次打开面板时（下面的
            // 观察者）才载入，扫描完成时刷新。
            let palette = Palette::new(cx, Vec::new(), matcher, on_accept, show);

            let search_model = SearchModel {
                palette,
                local_items: Arc::new(Vec::new()),
                load_generation: 0,
                local_items_loaded: false,
                #[cfg(feature = "kugou")]
                kugou_items: Vec::new(),
                #[cfg(feature = "kugou")]
                kugou_query_generation: 0,
            };

            #[cfg(feature = "online_sources")]
            {
                // 面板发出的是原始查询字符串；用它取在线结果并合并到本地结果之后
                let palette = search_model.palette.clone();
                cx.subscribe(
                    &palette,
                    |this: &mut SearchModel, _, query: &SharedString, cx| {
                        #[cfg(feature = "kugou")]
                        this.on_kugou_query(query, cx);
                    },
                )
                .detach();
            }

            // 首次打开搜索面板时在后台构建本地索引并推给调色板（按需构建，避免
            // 启动即常驻，也避免打开面板那一帧被三个全表查询 + 逐路径 stat 卡死）
            let show_for_load = show.clone();
            cx.observe(&show_for_load, move |this, show, cx| {
                if *show.read(cx) && !this.local_items_loaded {
                    this.load_local_items_async(cx);
                }
            })
            .detach();

            let scan_status = cx.global::<Models>().scan_state.clone();
            let show_for_scan = show.clone();

            cx.observe(&scan_status, move |this, scan_event, cx| {
                let state = scan_event.read(cx);

                if matches!(
                    *state,
                    ScanEvent::ScanCompleteIdle
                        | ScanEvent::ScanCompleteWatching
                        | ScanEvent::TargetedRescanComplete
                ) {
                    debug!("Scan complete, refreshing search items");

                    // 文件监视器的重扫即使在面板关闭时也会触发：那时重建整个索引
                    // 等于为没人看的结果查一遍全库。
                    //
                    // 成功才交换：这里**不**清空上一份索引——它保持安装状态，直到新
                    // 重建落地并整体交换 `Arc`（见 `load_local_items_async`）。
                    // 过去先清空会让本地那一段在整个数秒的重建期保持空着，于是那段
                    // 时间搜索只浮现在线结果。代价是：重建落地前，结果可能短暂引用
                    // 这次重扫刚删掉的文件——几条过期命中好过本地结果全黑好几秒。
                    // 重建失败（DB 错误）现在同样保留旧索引；旧的"先清空再失败"路径
                    // 会让它一直空到下一次
                    // 成功重建之后。
                    //
                    // `local_items_loaded = false` 只是重新武装上面那个面板打开
                    // 观察者去触发一次重建；没有任何渲染路径或空状态分支读它，期间
                    // 那份过期但有效的索引照常显示。递增代号会丢掉任何在途载入，于是
                    // 结果永远不会乱序落地。
                    this.local_items_loaded = false;
                    this.load_generation += 1;
                    if !*show_for_scan.read(cx) {
                        return;
                    }

                    this.load_local_items_async(cx);
                }
            })
            .detach();

            search_model
        })
    }

    /// 稳定的本地段后面接易变的在线（酷狗）结果。发出这个只会克隆在线那几页——
    /// 本地前缀作为一个 `Arc` 重新共享，而不是逐条目（见 `SplitItems`）。
    fn merged_items(&self) -> SplitItems<SearchPaletteItem> {
        SplitItems {
            stable: self.local_items.clone(),
            volatile: self.kugou_iter().cloned().collect(),
        }
    }

    /// 在后台 runtime 上载入本地索引，落地时把合并后的条目集推给面板。载入在途期间
    /// 在线结果可能已经到达（面板可以立刻被查询），所以应用时发出的是完整合并——
    /// 只发本地那部分会把它们丢掉。更新的载入代号会丢弃本次的结果。
    ///
    /// 成功才交换：新索引作为整体 `Arc` 赋值应用（在那之前旧的一直为 `merged_items`
    /// 服务），且只在 `Ok` 时——`Err` 的重建会让上一份索引保持安装，并让
    /// `local_items_loaded` 停在 false，好让下一次面板打开重试，而不是换成一段空的。
    fn load_local_items_async(&mut self, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let new_items = crate::RUNTIME
                .spawn(async move { load_search_items_off_thread(pool).await })
                .await
                .unwrap_or_else(|e| Err(format!("search index task failed: {e:?}")));

            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match new_items {
                    Ok(items) => {
                        this.local_items = Arc::new(items);
                        this.local_items_loaded = true;
                        let emitted = this.merged_items();
                        this.palette.update(cx, |_, cx| {
                            cx.emit(emitted);
                        });
                    }
                    Err(err) => {
                        // 保留上一份索引（见上面的成功才交换）；
                        // `local_items_loaded` 保持 false，好让下一次面板打开 /
                        // 扫描完成时重试。
                        debug!("Failed to rebuild search index, keeping previous one: {err}");
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    #[cfg(feature = "kugou")]
    fn kugou_iter(&self) -> impl Iterator<Item = &Arc<SearchPaletteItem>> {
        self.kugou_items.iter()
    }

    #[cfg(not(feature = "kugou"))]
    #[allow(dead_code)]
    fn kugou_iter(&self) -> std::iter::Empty<&Arc<SearchPaletteItem>> {
        std::iter::empty()
    }

    #[cfg(feature = "online_sources")]
    fn emit_merged_items(&self, cx: &mut Context<Self>) {
        let merged = self.merged_items();
        self.palette.update(cx, |_, cx| {
            cx.emit(merged);
        });
    }

    /// 防抖的在线搜索：最后一次击键 350ms 后查询酷狗，并把结果合并到本地结果之后。
    #[cfg(feature = "kugou")]
    fn on_kugou_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.on_online_query::<KugouSearch>(query, cx);
    }

    /// `on_kugou_query` 背后的共享骨架：递增该 provider 的查询代号，为过短的查询丢掉
    /// 过期结果，然后最后一次击键 350ms 后抓取、解析并把结果合并到本地结果之后。
    /// provider 的状态与接口由 `P` 提供，好让每个 feature 都能独立编译。
    #[cfg(feature = "kugou")]
    fn on_online_query<P: OnlineSearchProvider>(&mut self, query: &str, cx: &mut Context<Self>) {
        use tracing::warn;

        let generation = P::bump_generation(self);

        let query = query.trim().to_string();
        if query.chars().count() < 2 {
            // 只有确实有东西消失时才重新合并，否则下面少于两个字符的每次击键都会
            // 把整个音乐库重发一遍给 finder
            P::clear_stale_items(self, cx);
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;

            // 更新的击键顶掉了这一次；在碰网络之前就退出，免得快速打字每字符发一个请求
            if this
                .update(cx, |this, _| P::current_generation(this) != generation)
                .unwrap_or(true)
            {
                return;
            }

            let request = crate::RUNTIME
                .spawn(async move { P::fetch(query).await })
                .await;

            this.update(cx, |this, cx| {
                if P::current_generation(this) != generation {
                    return;
                }

                match request {
                    Ok(Ok(response)) => {
                        P::store_items(this, P::parse_items(&response));
                        this.emit_merged_items(cx);
                    }
                    // 搜索失败时保留上一次的结果：把它们清空会让面板在网络瞬时
                    // 出错时闪成空白，而这些过期条目反正会在下一次代号变化/重置时被丢掉
                    Ok(Err(err)) => {
                        warn!("{} online search failed: {err}", P::LABEL);
                    }
                    Err(err) => {
                        warn!("{} online search task failed: {err}", P::LABEL);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        cx.update_entity(&self.palette, |palette, cx| {
            palette.reset(cx);
        });

        // 丢掉在线结果，免得它们带着空查询又冒出来
        #[cfg(feature = "kugou")]
        {
            self.kugou_items.clear();
            self.kugou_query_generation += 1;
        }
        #[cfg(feature = "online_sources")]
        self.emit_merged_items(cx);
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.update(cx, |palette, cx| {
            palette.focus(window, cx);
        });
    }
}

/// `on_online_query` 用来触达逐 provider 搜索状态、客户端接口与条目构建的钩子。
/// 每个实现都待在它自己的 feature 下，好让 `kugou` 保持独立可编译。
#[cfg(feature = "kugou")]
trait OnlineSearchProvider: 'static {
    /// 在失败日志行里原样使用的标签。
    const LABEL: &'static str;

    /// 该 provider 搜索接口的错误类型。
    type Error: std::fmt::Display + Send + 'static;

    /// 递增并返回该 provider 的查询代号。
    fn bump_generation(model: &mut SearchModel) -> u64;
    /// 丢掉该 provider 的过期结果（若有）并重发合并结果。
    fn clear_stale_items(model: &mut SearchModel, cx: &mut Context<SearchModel>);
    /// 该 provider 当前的查询代号。
    fn current_generation(model: &SearchModel) -> u64;
    /// 把解析好的结果存进该 provider 的条目缓冲。
    fn store_items(model: &mut SearchModel, items: Vec<Arc<SearchPaletteItem>>);
    /// 运行该 provider 的搜索接口，解析到响应体。
    fn fetch(
        query: String,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, Self::Error>> + Send;
    /// 从响应体构建面板条目。
    fn parse_items(body: &serde_json::Value) -> Vec<Arc<SearchPaletteItem>>;
}

#[cfg(feature = "kugou")]
struct KugouSearch;

#[cfg(feature = "kugou")]
impl OnlineSearchProvider for KugouSearch {
    type Error = crate::kugou::KugouError;

    const LABEL: &'static str = "kugou";

    fn bump_generation(model: &mut SearchModel) -> u64 {
        model.kugou_query_generation += 1;
        model.kugou_query_generation
    }

    fn clear_stale_items(model: &mut SearchModel, cx: &mut Context<SearchModel>) {
        if !model.kugou_items.is_empty() {
            model.kugou_items.clear();
            model.emit_merged_items(cx);
        }
    }

    fn current_generation(model: &SearchModel) -> u64 {
        model.kugou_query_generation
    }

    fn store_items(model: &mut SearchModel, items: Vec<Arc<SearchPaletteItem>>) {
        model.kugou_items = items;
    }

    async fn fetch(query: String) -> Result<serde_json::Value, Self::Error> {
        crate::kugou::shared_client()
            .search(&query, 1, 15)
            .await
            .map(|response| response.body)
    }

    fn parse_items(body: &serde_json::Value) -> Vec<Arc<SearchPaletteItem>> {
        crate::ui::kugou::parse_tracks(body, "/data/lists")
            .into_iter()
            .map(|track| {
                let search_text: Arc<str> = format!("{} {}", track.title, track.artist).into();
                Arc::new(SearchPaletteItem::KugouTrack { track, search_text })
            })
            .collect()
    }
}

impl EventEmitter<ViewSwitchMessage> for SearchModel {}

impl Render for SearchModel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.palette.clone()
    }
}
