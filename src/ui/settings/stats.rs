use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use cntp_i18n::tr;
use gpui::{
    App, AppContext, Bounds, Context, Div, Entity, InteractiveElement, IntoElement, ParentElement,
    PathBuilder, Render, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, Window,
    canvas, div, fill, point, px, relative, size,
};

use crate::{
    stats::queries,
    ui::{
        app::Pool,
        components::{section_header::section_header, tooltip::build_tooltip},
        theme::Theme,
    },
};

/// 热力图几何：周一至周日共 7 行，9 px 单元格配 2 px 间隙。列数取自然年的周数
/// （52-53），于是网格约 580 px 宽，不用滚动就能放进设置窗口。
const HEAT_ROWS: usize = 7;
const HEAT_CELL_PX: f32 = 9.0;
const HEAT_GAP_PX: f32 = 2.0;
/// 热力图星期标签列的宽度。
const HEAT_WD_W_PX: f32 = 24.0;

const RANKS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];
const HOUR_LABELS: [&str; 4] = ["0", "6", "12", "18"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopTab {
    Tracks,
    Artists,
    Albums,
}

impl TopTab {
    fn id(self) -> &'static str {
        match self {
            Self::Tracks => "stats-tab-tracks",
            Self::Artists => "stats-tab-artists",
            Self::Albums => "stats-tab-albums",
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Tracks => tr!("STATS_TAB_TRACKS", "Tracks").into(),
            Self::Artists => tr!("STATS_TAB_ARTISTS", "Artists").into(),
            Self::Albums => tr!("STATS_TAB_ALBUMS", "Albums").into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopRange {
    Week,
    Month,
    All,
}

impl TopRange {
    fn id(self) -> &'static str {
        match self {
            Self::Week => "stats-range-week",
            Self::Month => "stats-range-month",
            Self::All => "stats-range-all",
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Week => tr!("STATS_RANGE_WEEK", "Last 7 days").into(),
            Self::Month => tr!("STATS_RANGE_MONTH", "Last 30 days").into(),
            Self::All => tr!("STATS_RANGE_ALL", "All time").into(),
        }
    }

    fn since(self) -> i64 {
        match self {
            Self::Week => chrono::Utc::now().timestamp() - 7 * 86_400,
            Self::Month => chrono::Utc::now().timestamp() - 30 * 86_400,
            Self::All => 0,
        }
    }
}

enum TopRows {
    Tracks(Vec<(String, String, i64)>),
    Artists(Vec<(String, i64)>),
    Albums(Vec<(String, i64)>),
}

/// 一行预计算好的 Top-10：所有字符串都在数据到达时构建，渲染路径永不格式化或分配。
struct TopItem {
    /// `top-row-{index}` 元素 id，与该行其余数据一起构建。
    row_id: SharedString,
    label: SharedString,
    sub: Option<SharedString>,
    duration: SharedString,
    pct: f32,
}

/// 热力图的一天：收听秒数，外加有收听的那些天预计算好的元素 id 与 tooltip，于是
/// 渲染永不格式化。颜色仍每帧从实时主题取（缓存它们会让浅色块在切换主题后漏进
/// 深色主题）。
struct HeatDay {
    secs: i64,
    /// 这一天有收听时间时的 `(元素 id, tooltip)`。
    active: Option<(SharedString, SharedString)>,
}

/// 热力图里的一周列；`secs == 0` 渲染成空格。
struct HeatWeek {
    days: [HeatDay; HEAT_ROWS],
}

fn fmt_duration(secs: i64) -> String {
    if secs >= 3600 {
        format!(
            "{:.1} {}",
            secs as f64 / 3600.0,
            tr!("STATS_UNIT_HOUR", "h")
        )
    } else if secs >= 60 {
        format!("{} {}", secs / 60, tr!("STATS_UNIT_MIN", "min"))
    } else {
        format!("{} {}", secs, tr!("STATS_UNIT_SEC", "s"))
    }
}

fn pct_of(secs: i64, max: i64) -> f32 {
    if max > 0 {
        secs as f32 / max as f32
    } else {
        0.0
    }
}

/// 到今天为止的连续收听天数（如果今天还是空的，则到昨天——不满一整天没收听，连续
/// 记录就不算断）。
fn compute_streak(daily: &BTreeMap<NaiveDate, i64>, today: NaiveDate) -> i64 {
    let mut day = today;
    if daily.get(&day).copied().unwrap_or(0) == 0 {
        day -= ChronoDuration::days(1);
    }
    let mut streak = 0;
    while daily.get(&day).copied().unwrap_or(0) > 0 {
        streak += 1;
        day -= ChronoDuration::days(1);
    }
    streak
}

/// 热力图的色阶。两套主题各有独立调色板，由卡片底色的亮度选定：
/// - 浅色：中性冷灰网格（在卡片色上只加一丝文字色——background_tertiary 被 371×
///   平铺时会发黄），配一个低锚点的强调色阶梯，在奶油底色上保持清利；
/// - 深色：在近黑卡片上做一次灰蓝中性抬升，阶梯更强，好让低档位仍能读出与背景的差别。
fn heat_color_scale(theme: &Theme) -> (Rgba, [Rgba; 4]) {
    let s = theme.background_secondary;
    let luma = 0.2126 * s.red + 0.7152 * s.green + 0.0722 * s.blue;
    let (empty, fractions) = if luma > 0.5 {
        (
            lerp_rgba(theme.background_secondary, theme.text, 0.05),
            [0.18f32, 0.40, 0.64, 1.0],
        )
    } else {
        // 深色：中性抬升略强一些，好让网格在近黑卡片上读得出来（参考风格的灰蓝方块）。
        (
            lerp_rgba(theme.background_secondary, theme.text, 0.09),
            [0.25f32, 0.50, 0.75, 1.0],
        )
    };
    let top = theme.button_primary;
    (empty, fractions.map(|t| lerp_rgba(empty, top, t)))
}

fn lerp_rgba(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba::new(
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
        a.alpha + (b.alpha - a.alpha) * t,
    )
}

/// 小时图子实体：只接管 `hover_hour` 状态与小时图元素。悬停列的 `cx.notify`
/// 落在本实体上，重绘仅限本图；热力图与 Top-10 留在 `StatsSettings`，
/// 不再因鼠标扫过 24 个悬停列而整页重建。
struct HoursChart {
    hours: [i64; 24],
    hours_max: i64,
    /// 预计算好的小时柱 tooltip 与元素 id。
    hours_labels: [Option<SharedString>; 24],
    hours_ids: [SharedString; 24],
    /// 归一化的折线点，每小时一个（x、y 取盒子 0..1 的比例）。
    hours_points: [(f32, f32); 24],
    /// 当前悬停的小时（驱动图表十字线），`None` = 无。
    hover_hour: Option<usize>,
}

impl HoursChart {
    /// 推入一份新的小时直方图（数据到达 / 30 秒刷新 tick）。
    fn apply_hours(&mut self, hours: [i64; 24], cx: &mut Context<Self>) {
        self.hours_max = hours.iter().copied().max().unwrap_or(0);
        self.hours = hours;
        self.hours_labels = std::array::from_fn(|hour| {
            let secs = hours[hour];
            (secs > 0).then(|| SharedString::from(format!("{hour:02}:00 · {}", fmt_duration(secs))))
        });
        self.hours_points = Self::normalized_points(&self.hours, self.hours_max);
        cx.notify();
    }

    /// 归一化的折线点，每小时一个（x、y 取盒子 0..1 的比例）。
    fn normalized_points(hours: &[i64; 24], hours_max: i64) -> [(f32, f32); 24] {
        let max = hours_max.max(1) as f32;
        std::array::from_fn(|hour| {
            (
                (hour as f32 + 0.5) / 24.0,
                0.93 - (hours[hour] as f32 / max) * 0.86,
            )
        })
    }
}

pub struct StatsSettings {
    loaded: bool,
    has_data: bool,
    daily: BTreeMap<NaiveDate, i64>,
    max_day_secs: i64,
    hours_chart: Entity<HoursChart>,
    /// 7 行（周一..周日）× 53 个周列；每天秒数，`None` = 未来。
    heat_weeks: Vec<HeatWeek>,
    /// 热力图窗口的第一天；渲染时的日期由它推导。
    heat_window_start: NaiveDate,
    /// 当前展示的年份；渲染时把窗口的周补齐部分（上一个十二月 / 下一个一月）裁掉。
    heat_year: i32,
    /// 月份标签：`(距网格原点的绝对 x 像素, 月份数字)`。
    month_items: Vec<(f32, SharedString)>,
    heat_window_label: SharedString,
    /// 年份翻页器：0 = 今年，1 = 去年，……
    heat_year_offset: i64,
    total_label: SharedString,
    today_label: SharedString,
    week_label: SharedString,
    streak_label: SharedString,
    top_tab: TopTab,
    top_range: TopRange,
    top_items: Vec<TopItem>,
    /// 每次 Top 查询启动时递增；跑完的查询若快照已不再匹配，说明它被更新的
    /// tab/范围切换顶掉了，应丢弃而不是覆盖更新的数据行。
    top_generation: u64,
    /// 最后一批已应用数据的 `(天行, 总秒数, 小时桶秒数, 本地日)`。两个 schema 事实
    /// 让"指纹一致"成为*精确*一致而非大概一致：`listen_event` 带
    /// `CHECK (seconds > 0)`（migrations/20260908000000_listen_stats.sql），且
    /// `src/`、`queries/`、`migrations/` 里没有任何地方 UPDATE 或 DELETE 它——于是
    /// 每次插入都严格抬高秒数总和，总和在一次真实变化后不可能相等。天那个分量覆盖
    /// 跨零点：数据静止时 `today_label`/`streak_label` 也由它派生。
    /// 一旦这两个事实变了（放宽 CHECK、加保留期清理），这个守卫就不再可证明，必须
    /// 改成按内容比较。
    applied_fingerprint: Option<(usize, i64, i64, NaiveDate)>,
}

impl StatsSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let entity = cx.new(|cx| Self {
            loaded: false,
            has_data: false,
            daily: BTreeMap::new(),
            max_day_secs: 0,
            hours_chart: cx.new(|_| HoursChart {
                hours: [0; 24],
                hours_max: 0,
                hours_labels: std::array::from_fn(|_| None),
                hours_ids: std::array::from_fn(|h| SharedString::from(format!("hb-{h}"))),
                hours_points: HoursChart::normalized_points(&[0; 24], 0),
                hover_hour: None,
            }),
            heat_weeks: Vec::new(),
            heat_window_start: Local::now().date_naive(),
            heat_year: Local::now().year(),
            month_items: Vec::new(),
            heat_window_label: "".into(),
            heat_year_offset: 0,
            total_label: "".into(),
            today_label: "".into(),
            week_label: "".into(),
            streak_label: "".into(),
            top_tab: TopTab::Tracks,
            top_range: TopRange::Week,
            top_items: Vec::new(),
            top_generation: 0,
            applied_fingerprint: None,
        });
        entity.update(cx, |this, cx| {
            this.load_base(cx, true);
            this.spawn_refresh(cx);
        });
        entity
    }

    /// 页面打开期间的实时刷新：播放时收听时间以 60 秒为单位落库，所以 30 秒重载
    /// 能让每个视图与真实值最多差一批（并在零点把 "today"/连续天数滚过去）。统计
    /// 分区实体只在本分区激活时存在——切走即 drop，这个循环也随之结束。
    fn spawn_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                // Err = 实体已释放；停止 tick。
                if this
                    .update(cx, |this, cx| this.load_base(cx, false))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// 日映射与小时直方图合成一次异步批次；Top 列表等基础数据（及其色阶）就位后再
    /// 跟上。30 秒刷新 tick 传 `with_top = false`——Top 列表只随 tab/范围变化，每
    /// tick 都重查只是为相同的行白烧一次 `GROUP BY`。
    /// 任一查询失败则记录 warn 并整批跳过
    /// apply（保留上次数据），失败不得折叠成权威空结果清掉 has_data。
    fn load_base(&mut self, cx: &mut Context<Self>, with_top: bool) {
        let pool = cx.global::<Pool>().0.clone();
        cx.spawn(async move |this, cx| {
            let base = crate::RUNTIME
                .spawn(async move {
                    let since = crate::stats::epoch_ts();
                    let daily = match queries::daily_sums(&pool, since).await {
                        Ok(rows) => rows,
                        Err(err) => {
                            tracing::warn!(?err, "stats: daily sums query failed");
                            return None;
                        }
                    };
                    let hours = match queries::hour_histogram(&pool, since).await {
                        Ok(rows) => rows,
                        Err(err) => {
                            tracing::warn!(?err, "stats: hour histogram query failed");
                            return None;
                        }
                    };
                    Some((daily, hours))
                })
                .await;
            if let Ok(Some((daily, hours))) = base {
                let _ = this.update(cx, |this, cx| {
                    let fingerprint = (
                        daily.len(),
                        daily.iter().map(|(_, total)| *total).sum::<i64>(),
                        hours.iter().sum::<i64>(),
                        Local::now().date_naive(),
                    );
                    // 数量 + 各项总和都匹配即为精确匹配（只追加的表），于是一无所变的
                    // tick 不重新解析日期、不重建热力图、也不请求重绘。
                    if this.applied_fingerprint != Some(fingerprint) {
                        this.applied_fingerprint = Some(fingerprint);
                        this.apply_base(daily, hours, cx);
                        cx.notify();
                    }
                    if with_top {
                        this.load_top(cx);
                    }
                });
            }
        })
        .detach();
    }

    fn apply_base(&mut self, daily: Vec<(String, i64)>, hours: [i64; 24], cx: &mut Context<Self>) {
        let mut map = BTreeMap::new();
        for (day, total) in daily {
            if let Ok(date) = NaiveDate::parse_from_str(&day, "%Y-%m-%d") {
                map.insert(date, total);
            }
        }
        self.has_data = !map.is_empty();
        self.max_day_secs = map.values().copied().max().unwrap_or(0);
        self.daily = map;

        self.hours_chart
            .update(cx, |chart, cx| chart.apply_hours(hours, cx));

        // 概览数字都从日映射派生（daily_sums 与热力图共用的唯一真相源）。
        let today = Local::now().date_naive();
        let today_secs = self.daily.get(&today).copied().unwrap_or(0);
        let week_secs: i64 = (0..7)
            .filter_map(|i| self.daily.get(&(today - ChronoDuration::days(i))).copied())
            .sum();
        let total_secs: i64 = self.daily.values().sum();
        self.today_label = fmt_duration(today_secs).into();
        self.week_label = fmt_duration(week_secs).into();
        self.total_label = fmt_duration(total_secs).into();
        self.streak_label = format!(
            "{} {}",
            compute_streak(&self.daily, today),
            tr!("STATS_UNIT_DAY", "d")
        )
        .into();
        self.rebuild_heat();
        self.loaded = true;
    }

    fn load_top(&mut self, cx: &mut Context<Self>) {
        self.top_generation += 1;
        let generation = self.top_generation;
        let pool = cx.global::<Pool>().0.clone();
        let (tab, since) = (self.top_tab, self.top_range.since());
        cx.spawn(async move |this, cx| {
            let rows = crate::RUNTIME
                .spawn(async move {
                    let query = match tab {
                        TopTab::Tracks => {
                            queries::top_tracks(&pool, since).await.map(TopRows::Tracks)
                        }
                        TopTab::Artists => queries::top_artists(&pool, since)
                            .await
                            .map(TopRows::Artists),
                        TopTab::Albums => {
                            queries::top_albums(&pool, since).await.map(TopRows::Albums)
                        }
                    };
                    // Top 失败时保留 self.top_items，不覆盖为空列表。
                    match query {
                        Ok(rows) => Some(rows),
                        Err(err) => {
                            tracing::warn!(?err, "stats: top query failed");
                            None
                        }
                    }
                })
                .await;
            if let Ok(Some(rows)) = rows {
                let _ = this.update(cx, |this, cx| {
                    // 更新的 tab/范围切换顶掉了这次查询；丢弃它的行，而不是覆盖更新
                    // 的数据。
                    if this.top_generation != generation {
                        return;
                    }
                    this.apply_top(rows);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn apply_top(&mut self, rows: TopRows) {
        fn to_items(rows: Vec<(String, Option<String>, i64)>) -> Vec<TopItem> {
            let max = rows.iter().map(|r| r.2).max().unwrap_or(0);
            rows.into_iter()
                .enumerate()
                .map(|(i, (label, sub, secs))| TopItem {
                    row_id: SharedString::from(format!("top-row-{i}")),
                    label: label.into(),
                    sub: sub.filter(|s| !s.is_empty()).map(Into::into),
                    duration: fmt_duration(secs).into(),
                    pct: pct_of(secs, max),
                })
                .collect()
        }

        self.top_items = match rows {
            TopRows::Tracks(rows) => to_items(
                rows.into_iter()
                    .map(|(title, artist, secs)| (title, Some(artist), secs))
                    .collect(),
            ),
            // 艺术家与专辑的渲染完全相同：一个名字，无副行。
            TopRows::Artists(rows) | TopRows::Albums(rows) => to_items(
                rows.into_iter()
                    .map(|(name, secs)| (name, None, secs))
                    .collect(),
            ),
        };
    }

    fn rebuild_heat(&mut self) {
        let today = Local::now().date_naive();
        // GitHub 风格的自然年窗口：列是覆盖所选年 1 月 1 日到 12 月 31 日的整周
        // （周一..周日）——今年也算在内；其中未来的日子就渲染成空格。
        // `heat_year_offset` 一次往前翻一年，到统计起始年就停，更早根本没有数据。
        let year = today.year() - self.heat_year_offset as i32;
        // chrono 支持范围之外的年份建不出热力图；此时保留当前年，而不是 panic。
        let (Some(year_start), Some(last_day)) = (
            NaiveDate::from_ymd_opt(year, 1, 1),
            NaiveDate::from_ymd_opt(year, 12, 31),
        ) else {
            tracing::warn!("invalid heatmap year {year}, keeping current heatmap");
            return;
        };
        let window_start = year_start
            - ChronoDuration::days(i64::from(year_start.weekday().num_days_from_monday()));
        let window_end_sunday = last_day
            + ChronoDuration::days(6 - i64::from(last_day.weekday().num_days_from_monday()));
        let cols = ((window_end_sunday - window_start).num_days() / 7 + 1).max(1);

        self.heat_window_label = year.to_string().into();
        self.heat_year = year;

        // 月份标签正好落在承载该月第一天的那一列上方。相邻月份的起始列恒相隔 4-5 列
        // （一个月 >= 28 天），所以用绝对 x 定位能让每个标签都与邻居错开——按游程
        // 排开的 spacer 会忽略已累计的标签宽度，把每个标签挤到上一个旁边。
        self.month_items = (1..=12)
            .filter_map(|month| {
                // months 1..=12 恒合法；这层守卫只覆盖 chrono 范围之外的年份
                let month_start = NaiveDate::from_ymd_opt(year, month, 1)?;
                let col = (month_start - window_start).num_days() / 7;
                Some((
                    col as f32 * (HEAT_CELL_PX + HEAT_GAP_PX),
                    month.to_string().into(),
                ))
            })
            .collect();

        // 逐周的日格子：有收听那天的 id + tooltip 在数据到达时就算好（渲染永不
        // 格式化）；颜色取自实时主题。
        let mut heat_weeks: Vec<HeatWeek> = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let mut days: [HeatDay; HEAT_ROWS] = std::array::from_fn(|_| HeatDay {
                secs: 0,
                active: None,
            });
            for row in 0..HEAT_ROWS as i64 {
                let date = window_start + ChronoDuration::days(col * 7 + row);
                if date > today {
                    continue;
                }
                let secs = self.daily.get(&date).copied().unwrap_or(0);
                let active = (secs > 0).then(|| {
                    (
                        SharedString::from(format!("hm-{}", date.format("%Y%m%d"))),
                        SharedString::from(format!(
                            "{} · {}",
                            fmt_duration(secs),
                            date.format("%m-%d")
                        )),
                    )
                });
                days[row as usize] = HeatDay { secs, active };
            }
            heat_weeks.push(HeatWeek { days });
        }
        self.heat_window_start = window_start;
        self.heat_weeks = heat_weeks;
    }

    fn shift_heat(&mut self, delta: i64, cx: &mut Context<Self>) {
        // 翻页器到统计起始年就停：更早的年份根本没有数据。
        let max_offset =
            (i64::from(Local::now().year()) - i64::from(crate::stats::epoch_year())).max(0);
        let next = (self.heat_year_offset + delta).clamp(0, max_offset);
        if next == self.heat_year_offset {
            return;
        }
        self.heat_year_offset = next;
        self.rebuild_heat();
        cx.notify();
    }

    fn switch_tab(&mut self, tab: TopTab, cx: &mut Context<Self>) {
        if self.top_tab == tab {
            return;
        }
        self.top_tab = tab;
        self.load_top(cx);
    }

    fn switch_range(&mut self, range: TopRange, cx: &mut Context<Self>) {
        if self.top_range == range {
            return;
        }
        self.top_range = range;
        self.load_top(cx);
    }

    fn render_overview(&self, theme: &Theme) -> impl IntoElement {
        let cards = [
            (
                SharedString::from(tr!("STATS_OVERVIEW_TOTAL", "Total")),
                self.total_label.clone(),
            ),
            (
                SharedString::from(tr!("STATS_OVERVIEW_TODAY", "Today")),
                self.today_label.clone(),
            ),
            (
                SharedString::from(tr!("STATS_OVERVIEW_WEEK", "Last 7 days")),
                self.week_label.clone(),
            ),
            (
                SharedString::from(tr!("STATS_OVERVIEW_STREAK", "Streak")),
                self.streak_label.clone(),
            ),
        ];
        div()
            .flex()
            .flex_row()
            .gap(px(10.0))
            .w_full()
            .children(cards.map(|(label, value)| {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .bg(theme.background_secondary)
                    .border_1()
                    .border_color(theme.border_color)
                    .rounded(px(theme.radius_md))
                    .px(px(14.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_secondary)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(17.0))
                            .text_color(theme.text)
                            .child(value),
                    )
            }))
    }

    fn render_heatmap(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let window_label = self.heat_window_label.clone();
        // 主题派生的颜色每帧解析：缓存它们会让浅色调色板在实时切换主题后漏进深色
        // 主题。
        let (empty, levels) = heat_color_scale(theme);

        // 标签用绝对定位放到该月首个周列的上方（flex spacer 量不到文字宽度，会逐月
        // 把一个标签挪位）。x 偏移 = 星期标签列 + 它的外边距。
        let month_row = div()
            .relative()
            .h(px(12.0))
            .children(self.month_items.iter().map(|(x, label)| {
                div()
                    .absolute()
                    .left(px(HEAT_WD_W_PX + HEAT_GAP_PX + *x))
                    .top(px(0.0))
                    .text_size(px(9.0))
                    .text_color(theme.text_secondary)
                    .child(label.clone())
            }));

        let weekday_labels: [Option<SharedString>; HEAT_ROWS] = [
            Some(tr!("STATS_WD_MON", "Mon").into()),
            None,
            Some(tr!("STATS_WD_WED", "Wed").into()),
            None,
            Some(tr!("STATS_WD_FRI", "Fri").into()),
            None,
            None,
        ];

        // 星期标签列位于各周列的左侧。
        let wd_col = div()
            .flex()
            .flex_col()
            .gap(px(HEAT_GAP_PX))
            .flex_shrink_0()
            .mr(px(HEAT_GAP_PX))
            .children(weekday_labels.map(|label| {
                let mut slot = div()
                    .w(px(HEAT_WD_W_PX))
                    .h(px(HEAT_CELL_PX))
                    .flex()
                    .items_center();
                if let Some(l) = label {
                    slot = slot.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_secondary)
                            .child(l),
                    );
                }
                slot
            }));

        // 窗口会补齐到整周（周一..周日）：展示年之外的日子（上一个十二月 / 下一个
        // 一月）保留格子位置以对齐列，但不可见渲染。
        // 哨兵值只为下面那个闭区间检查而存在：chrono 范围之外的年份（不可达——
        // rebuild_heat 已校验）会把每个格子都当作在窗口内渲染，而不是 panic
        // （哨兵接受所有真实日期）。
        let year_start = NaiveDate::from_ymd_opt(self.heat_year, 1, 1).unwrap_or(NaiveDate::MIN);
        let year_end = NaiveDate::from_ymd_opt(self.heat_year, 12, 31).unwrap_or(NaiveDate::MAX);

        // 格子是有状态的（日 tooltip，停留 150 ms 才弹出——鼠标快速划过不会触发）；
        // 列只是普通布局。
        let mut grid = div().flex().flex_row().gap(px(HEAT_GAP_PX));
        for (col, week) in self.heat_weeks.iter().enumerate() {
            let mut col_div = div().flex().flex_col().gap(px(HEAT_GAP_PX));
            for (row, day) in week.days.iter().enumerate() {
                let date =
                    self.heat_window_start + ChronoDuration::days(col as i64 * 7 + row as i64);
                let base = div()
                    .w(px(HEAT_CELL_PX))
                    .h(px(HEAT_CELL_PX))
                    .rounded(px(2.0))
                    .flex_shrink_0();
                let base = if date >= year_start && date <= year_end {
                    // 未来的日子共用空格的颜色
                    let color = if day.secs == 0 {
                        empty
                    } else {
                        let ratio = if self.max_day_secs > 0 {
                            day.secs as f32 / self.max_day_secs as f32
                        } else {
                            1.0
                        };
                        levels[match ratio {
                            r if r > 0.75 => 3,
                            r if r > 0.5 => 2,
                            r if r > 0.25 => 1,
                            _ => 0,
                        }]
                    };
                    base.bg(color)
                } else {
                    base
                };
                // 有收听那天的 id + tooltip 早在数据到达时就算好了。
                let cell = if let Some((id, tooltip)) = &day.active {
                    base.id(id.clone())
                        .tooltip_show_delay(Duration::from_millis(150))
                        .tooltip(build_tooltip(tooltip.clone()))
                        .into_any_element()
                } else {
                    base.into_any_element()
                };
                col_div = col_div.child(cell);
            }
            grid = grid.child(col_div);
        }

        let legend = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(3.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("STATS_LEGEND_LESS", "Less")),
            )
            .children(
                std::iter::once(empty)
                    .chain(levels.iter().copied())
                    .map(|color| {
                        div()
                            .w(px(HEAT_CELL_PX))
                            .h(px(HEAT_CELL_PX))
                            .rounded(px(2.0))
                            .bg(color)
                    }),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("STATS_LEGEND_MORE", "More")),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("STATS_HEATMAP_TITLE", "Listening activity")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_secondary)
                            .child(window_label),
                    )
                    .child(
                        // ‹ 往更早的年份翻（+1），› 往今天方向回退。
                        self.pager_button("stats-heat-prev", "<", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.shift_heat(1, cx))),
                    )
                    .child(
                        self.pager_button("stats-heat-next", ">", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.shift_heat(-1, cx))),
                    ),
            )
            .child(month_row)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .child(wd_col)
                    .child(grid),
            )
            .child(legend)
    }

    fn pager_button(&self, id: &'static str, glyph: &'static str, theme: &Theme) -> Stateful<Div> {
        let hover_bg = theme.menu_item_hover;
        let idle_text = theme.text_secondary;
        let hover_text = theme.text;
        div()
            .id(id)
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(theme.radius_sm))
            .bg(theme.background_secondary)
            .border_1()
            .border_color(theme.border_color)
            .text_size(px(11.0))
            .text_color(idle_text)
            .hover(move |s| s.bg(hover_bg).text_color(hover_text))
            .child(glyph)
    }

    fn render_top(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_row = div().flex().flex_row().gap(px(4.0)).children(
            [TopTab::Tracks, TopTab::Artists, TopTab::Albums].map(|tab| {
                self.pill(tab.id(), tab.label(), self.top_tab == tab, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.switch_tab(tab, cx)))
            }),
        );
        let range_row = div().flex().flex_row().gap(px(4.0)).children(
            [TopRange::Week, TopRange::Month, TopRange::All].map(|range| {
                self.pill(range.id(), range.label(), self.top_range == range, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.switch_range(range, cx)))
            }),
        );

        let mut rows = div().flex().flex_col().w_full();
        // 与小时柱同一款保证对比度的着色：menu_item_hover 在浅色模式下对本页底色
        // 完全不可见。
        let row_hover = lerp_rgba(theme.background_secondary, theme.text, 0.045);
        for (i, item) in self.top_items.iter().enumerate() {
            // 有状态的行：hover 样式只在带 id 的元素上才会重绘（元素状态驱动
            // enter/leave 的 notify）。
            let mut row = div()
                .id(item.row_id.clone())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .w_full()
                .h(px(26.0))
                .rounded(px(4.0))
                .hover(move |s| s.bg(row_hover))
                .child(
                    div()
                        .w(px(16.0))
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary)
                        .child(RANKS[i]),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .text_ellipsis()
                        .child(item.label.clone()),
                );
            if let Some(sub) = &item.sub {
                row = row.child(
                    div()
                        .w(px(120.0))
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary)
                        .text_ellipsis()
                        .child(sub.clone()),
                );
            }
            row = row
                .child(
                    div()
                        .w(px(80.0))
                        .h(px(4.0))
                        .flex_shrink_0()
                        .rounded(px(2.0))
                        .bg(theme.background_tertiary)
                        .child(
                            div()
                                .h_full()
                                .rounded(px(2.0))
                                .bg(theme.button_primary)
                                .w(relative(item.pct)),
                        ),
                )
                .child(
                    div()
                        .w(px(56.0))
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary)
                        .child(item.duration.clone()),
                );
            rows = rows.child(row);
        }
        if self.top_items.is_empty() {
            rows = rows.child(
                div()
                    .pt(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_disabled)
                    .child(tr!("STATS_EMPTY")),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("STATS_TOP_TITLE", "Top 10")),
            )
            .child(tab_row)
            .child(range_row)
            .child(rows)
    }

    fn pill(
        &self,
        id: &'static str,
        label: SharedString,
        active: bool,
        theme: &Theme,
    ) -> Stateful<Div> {
        let (bg, fg) = if active {
            (theme.background_tertiary, theme.text)
        } else {
            (theme.background_secondary, theme.text_secondary)
        };
        let hover_bg = theme.menu_item_hover;
        div()
            .id(id)
            .px(px(10.0))
            .py(px(3.0))
            .rounded(px(theme.radius_sm))
            .bg(bg)
            .border_1()
            .border_color(theme.border_color)
            .text_size(px(11.0))
            .text_color(fg)
            .hover(move |s| s.bg(hover_bg))
            .child(label)
    }
}

impl Render for HoursChart {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 主题派生的颜色每帧解析：缓存它们会让浅色调色板在实时切换主题后漏进深色
        // 主题。
        let theme = cx.global::<Theme>();
        let points = self.hours_points;

        let area_color = lerp_rgba(theme.background_secondary, theme.button_primary, 0.14);
        let line_color = theme.button_primary;
        let guide_color = lerp_rgba(line_color, theme.background_secondary, 0.6);

        // 交互覆盖层：24 个透明列驱动十字线（写入 `hover_hour`；canvas 在数据点上
        // 画参考线 + 标记）。不做列填充：高亮一列看起来是柱状图的 hover，用在折线图
        // 上是错的。
        let entity = cx.entity();
        let mut overlay = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .flex_row();
        for hour in 0..24 {
            let entity = entity.clone();
            let base = div()
                .id(self.hours_ids[hour].clone())
                .flex_1()
                .min_w_0()
                .h_full()
                .on_hover(move |hovered: &bool, _, cx| {
                    entity.update(cx, |this, cx| {
                        let next = hovered.then_some(hour);
                        if this.hover_hour != next {
                            this.hover_hour = next;
                            cx.notify();
                        }
                    });
                });
            let col = if let Some(text) = &self.hours_labels[hour] {
                base.tooltip_show_delay(Duration::from_millis(150))
                    .tooltip(build_tooltip(text.clone()))
                    .into_any_element()
            } else {
                base.into_any_element()
            };
            overlay = overlay.child(col);
        }

        // 曲线下填充 + 2 px 线 + 十字线，画在 canvas 上。
        let paint_entity = entity;
        let chart = canvas(
            move |bounds, _, _| bounds,
            move |bounds, _, window, cx| {
                let left = f32::from(bounds.left());
                let top = f32::from(bounds.top());
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                let at = |(x, y): &(f32, f32)| point(px(left + x * w), px(top + y * h));

                let mut area = PathBuilder::fill();
                area.move_to(point(px(left), px(top + h)));
                for p in &points {
                    area.line_to(at(p));
                }
                area.line_to(point(px(left + w), px(top + h)));
                area.close();
                if let Ok(path) = area.build() {
                    window.paint_path(path, area_color);
                }

                let mut curve = PathBuilder::stroke(px(2.0));
                let mut iter = points.iter();
                if let Some(p) = iter.next() {
                    curve.move_to(at(p));
                    for p in iter {
                        curve.line_to(at(p));
                    }
                }
                if let Ok(path) = curve.build() {
                    window.paint_path(path, line_color);
                }

                // 十字线：在悬停小时的参考线 + 标记点。
                let hover_hour = paint_entity.read(cx).hover_hour;
                if let Some(hour) = hover_hour
                    && let Some(&(x, y)) = points.get(hour)
                {
                    let px_x = left + x * w;
                    let px_y = top + y * h;
                    let mut guide = PathBuilder::stroke(px(1.0));
                    guide.move_to(point(px(px_x), px(px_y)));
                    guide.line_to(point(px(px_x), px(top + h)));
                    if let Ok(path) = guide.build() {
                        window.paint_path(path, guide_color);
                    }
                    window.paint_quad(
                        fill(
                            Bounds::new(
                                point(px(px_x - 3.0), px(px_y - 3.0)),
                                size(px(6.0), px(6.0)),
                            ),
                            line_color,
                        )
                        .corner_radii(px(3.0)),
                    );
                }
            },
        )
        .size_full();

        // 图表下方的小时标签。
        let labels = div()
            .flex()
            .flex_row()
            .mt(px(2.0))
            .children((0..24).map(|hour| {
                let mut slot = div().flex_1().min_w_0().flex().justify_center();
                if hour % 6 == 0 {
                    slot = slot.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_secondary)
                            .child(HOUR_LABELS[hour / 6]),
                    );
                }
                slot
            }));

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("STATS_HOURS_TITLE", "By hour of day")),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(80.0))
                    .child(chart)
                    .child(overlay),
            )
            .child(labels)
    }
}

impl Render for StatsSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();

        if self.loaded && !self.has_data {
            return div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(section_header(tr!("STATS_SECTION")))
                .child(
                    div().w_full().flex().justify_center().pt(px(40.0)).child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("STATS_EMPTY", "No listening data yet.")),
                    ),
                );
        }

        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section_header(tr!("STATS_SECTION")));
        if self.loaded {
            section = section
                .child(self.render_overview(&theme))
                .child(self.render_heatmap(&theme, cx))
                .child(self.hours_chart.clone())
                .child(self.render_top(&theme, cx));
        }
        section
    }
}
