use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use cntp_i18n::tr;
use gpui::{
    canvas, fill, point, relative, size, App, AppContext, Bounds, Context, Div, Entity,
    InteractiveElement, IntoElement, ParentElement, PathBuilder, Render, Rgba, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::{
    stats::queries,
    ui::{
        app::Pool,
        components::{section_header::section_header, tooltip::build_tooltip},
        theme::Theme,
    },
};

/// Heat-map geometry: Monday..Sunday rows, 9 px cells with a 2 px gap. The
/// column count is the natural year's week count (52-53), so the grid is
/// ~580 px wide and fits the settings window without scrolling.
const HEAT_ROWS: usize = 7;
const HEAT_CELL_PX: f32 = 9.0;
const HEAT_GAP_PX: f32 = 2.0;
/// Width of the heat-map weekday label column.
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

/// One precomputed Top-10 row: all strings are built when data arrives so the
/// render path never formats or allocates.
struct TopItem {
    label: SharedString,
    sub: Option<SharedString>,
    duration: SharedString,
    pct: f32,
}

/// One heat-map week column: per-day seconds (`None` = future date). Colors
/// are derived per frame from the live theme so theme switches apply instantly
/// (caching them made light cells leak into the dark theme after a switch);
/// the week tooltip lives in `week_tooltips`.
struct HeatWeek {
    days: [Option<i64>; HEAT_ROWS],
}

fn fmt_duration(secs: i64) -> String {
    if secs >= 3600 {
        format!("{:.1} {}", secs as f64 / 3600.0, tr!("STATS_UNIT_HOUR", "h"))
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

/// Consecutive listening days ending today (or yesterday, if today is still
/// empty — the streak is not dead until a full day passes unlistened).
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

/// Color scale for the heat map. The two themes get independent palettes,
/// chosen by the card tone's luma:
/// - light: a neutral cold-gray grid (a whisper of text over the card color —
///   background_tertiary yellows when tiled 371×) with a low-anchor accent
///   ladder that stays crisp on the cream background;
/// - dark: a gray-blue neutral lift over the near-black card with a stronger
///   ladder, so the low steps still read against the background.
fn heat_color_scale(theme: &Theme) -> (Rgba, [Rgba; 4]) {
    let s = theme.background_secondary;
    let luma = 0.2126 * s.red + 0.7152 * s.green + 0.0722 * s.blue;
    let (empty, fractions) = if luma > 0.5 {
        (
            lerp_rgba(theme.background_secondary, theme.text, 0.05),
            [0.18f32, 0.40, 0.64, 1.0],
        )
    } else {
        // Dark: a slightly stronger neutral lift so the grid reads against the
        // near-black card (reference-style gray-blue squares).
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

pub struct StatsSettings {
    loaded: bool,
    has_data: bool,
    daily: BTreeMap<NaiveDate, i64>,
    max_day_secs: i64,
    hours: [i64; 24],
    hours_max: i64,
    /// Precomputed hour-bar tooltips and element ids.
    hours_labels: [Option<SharedString>; 24],
    hours_ids: [SharedString; 24],
    /// Hour currently hovered (drives the chart crosshair), `None` = none.
    hover_hour: Option<usize>,
    /// 7 rows (Mon..Sun) × 53 week columns; per-day seconds, `None` = future.
    heat_weeks: Vec<HeatWeek>,
    /// First day of the heat-map window; day dates derive from it at render.
    heat_window_start: NaiveDate,
    /// Shown year; render clips the window's week padding (previous December /
    /// next January) to it.
    heat_year: i32,
    /// Month labels: (absolute x px from the grid origin, month number).
    month_items: Vec<(f32, SharedString)>,
    heat_window_label: SharedString,
    /// Year pager: 0 = current year, 1 = last year, …
    heat_year_offset: i64,
    total_label: SharedString,
    today_label: SharedString,
    week_label: SharedString,
    streak_label: SharedString,
    top_tab: TopTab,
    top_range: TopRange,
    top_items: Vec<TopItem>,
}

impl StatsSettings {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let entity = cx.new(|_| Self {
            loaded: false,
            has_data: false,
            daily: BTreeMap::new(),
            max_day_secs: 0,
            hours: [0; 24],
            hours_max: 0,
            hours_labels: std::array::from_fn(|_| None),
            hours_ids: std::array::from_fn(|h| SharedString::from(format!("hb-{h}"))),
            hover_hour: None,
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
        });
        entity.update(cx, |this, cx| {
            this.load_base(cx);
            this.spawn_refresh(cx);
        });
        entity
    }

    /// Live refresh while the page is open: listening time lands in the DB in
    /// 60 s quanta while playing, so a 30 s reload keeps every view within one
    /// batch of the truth (and rolls "today"/streak over at midnight). The
    /// stats section entity only lives while this section is active —
    /// switching away drops it and ends this loop.
    fn spawn_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                // Err = entity released; stop ticking.
                if this
                    .update(cx, |this, cx| this.load_base(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// One async batch for the daily map and the hour histogram; the Top list
    /// follows once the base data (and its color scale) is in place.
    fn load_base(&mut self, cx: &mut Context<Self>) {
        let pool = cx.global::<Pool>().0.clone();
        cx.spawn(async move |this, cx| {
            let base = crate::RUNTIME
                .spawn(async move {
                    let since = crate::stats::epoch_ts();
                    let daily = queries::daily_sums(&pool, since).await.unwrap_or_default();
                    let hours = queries::hour_histogram(&pool, since).await.unwrap_or_default();
                    (daily, hours)
                })
                .await;
            if let Ok((daily, hours)) = base {
                let _ = this.update(cx, |this, cx| {
                    this.apply_base(daily, hours, cx);
                    this.load_top(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn apply_base(
        &mut self,
        daily: Vec<(String, i64)>,
        hours: [i64; 24],
        _cx: &mut Context<Self>,
    ) {
        let mut map = BTreeMap::new();
        for (day, total) in daily {
            if let Ok(date) = NaiveDate::parse_from_str(&day, "%Y-%m-%d") {
                map.insert(date, total);
            }
        }
        self.has_data = !map.is_empty();
        self.max_day_secs = map.values().copied().max().unwrap_or(0);
        self.daily = map;

        self.hours_max = hours.iter().copied().max().unwrap_or(0);
        self.hours = hours;
        self.hours_labels = std::array::from_fn(|hour| {
            let secs = hours[hour];
            (secs > 0).then(|| {
                SharedString::from(format!("{hour:02}:00 · {}", fmt_duration(secs)))
            })
        });

        // Overview numbers are derived from the daily map (single source of
        // truth for daily_sums and the heat map alike).
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
        let pool = cx.global::<Pool>().0.clone();
        let (tab, since) = (self.top_tab, self.top_range.since());
        cx.spawn(async move |this, cx| {
            let rows = crate::RUNTIME
                .spawn(async move {
                    match tab {
                        TopTab::Tracks => TopRows::Tracks(
                            queries::top_tracks(&pool, since)
                                .await
                                .unwrap_or_default(),
                        ),
                        TopTab::Artists => TopRows::Artists(
                            queries::top_artists(&pool, since)
                                .await
                                .unwrap_or_default(),
                        ),
                        TopTab::Albums => TopRows::Albums(
                            queries::top_albums(&pool, since)
                                .await
                                .unwrap_or_default(),
                        ),
                    }
                })
                .await;
            if let Ok(rows) = rows {
                let _ = this.update(cx, |this, cx| {
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
                .map(|(label, sub, secs)| TopItem {
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
            TopRows::Artists(rows) => {
                to_items(rows.into_iter().map(|(name, secs)| (name, None, secs)).collect())
            }
            TopRows::Albums(rows) => {
                to_items(rows.into_iter().map(|(name, secs)| (name, None, secs)).collect())
            }
        };
    }

    fn rebuild_heat(&mut self) {
        let today = Local::now().date_naive();
        // GitHub-style natural-year window: columns are whole weeks (Mon..Sun)
        // covering Jan 1 through Dec 31 of the selected year — the current
        // year included; its future days just render as empty cells.
        // `heat_year_offset` pages back one year at a time, stopping at the
        // stats epoch year, before which there is no data at all.
        let year = today.year() - self.heat_year_offset as i32;
        let year_start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        let last_day = NaiveDate::from_ymd_opt(year, 12, 31).unwrap();
        let window_start =
            year_start - ChronoDuration::days(i64::from(year_start.weekday().num_days_from_monday()));
        let window_end_sunday = last_day
            + ChronoDuration::days(6 - i64::from(last_day.weekday().num_days_from_monday()));
        let cols = ((window_end_sunday - window_start).num_days() / 7 + 1).max(1);

        self.heat_window_label = year.to_string().into();
        self.heat_year = year;

        // Month labels sit exactly above the column holding each month's
        // first day. Consecutive month starts are always 4-5 columns apart
        // (a month is >= 28 days), so absolute x positions keep every label
        // clear of its neighbours — run-length spacers ignored the
        // accumulated label widths and crowded each label next to the
        // previous one.
        self.month_items = (1..=12)
            .map(|month| {
                let month_start = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
                let col = (month_start - window_start).num_days() / 7;
                (
                    col as f32 * (HEAT_CELL_PX + HEAT_GAP_PX),
                    month.to_string().into(),
                )
            })
            .collect();

        // Per-week day seconds (`None` = future date). Day tooltips are
        // derived in `render_heatmap` from `heat_window_start`; colors come
        // from the live theme.
        let mut heat_weeks: Vec<HeatWeek> = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let mut days = [None; HEAT_ROWS];
            for row in 0..HEAT_ROWS as i64 {
                let date = window_start + ChronoDuration::days(col * 7 + row);
                if date > today {
                    continue;
                }
                days[row as usize] = Some(self.daily.get(&date).copied().unwrap_or(0));
            }
            heat_weeks.push(HeatWeek { days });
        }
        self.heat_window_start = window_start;
        self.heat_weeks = heat_weeks;
    }

    fn shift_heat(&mut self, delta: i64, cx: &mut Context<Self>) {
        // The pager stops at the stats epoch year: earlier years hold no data.
        let max_offset = (i64::from(Local::now().year()) - i64::from(crate::stats::epoch_year())).max(0);
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
            (SharedString::from(tr!("STATS_OVERVIEW_TOTAL", "Total")), self.total_label.clone()),
            (SharedString::from(tr!("STATS_OVERVIEW_TODAY", "Today")), self.today_label.clone()),
            (SharedString::from(tr!("STATS_OVERVIEW_WEEK", "Last 7 days")), self.week_label.clone()),
            (SharedString::from(tr!("STATS_OVERVIEW_STREAK", "Streak")), self.streak_label.clone()),
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
                    .child(div().text_size(px(17.0)).text_color(theme.text).child(value))
            }))
    }

    fn render_heatmap(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let window_label = self.heat_window_label.clone();
        // Theme-derived colors resolve per frame: caching them leaked the
        // light palette into the dark theme after a live theme switch.
        let (empty, levels) = heat_color_scale(theme);

        // Labels are absolutely positioned above their month's first week
        // column (flex spacers can't measure text width and drift a label per
        // month). x offset = weekday label column + its margin.
        let mut month_row = div().relative().h(px(12.0));
        for (x, label) in &self.month_items {
            month_row = month_row.child(
                div()
                    .absolute()
                    .left(px(HEAT_WD_W_PX + HEAT_GAP_PX + *x))
                    .top(px(0.0))
                    .text_size(px(9.0))
                    .text_color(theme.text_secondary)
                    .child(label.clone()),
            );
        }

        let weekday_labels: [Option<SharedString>; HEAT_ROWS] = [
            Some(tr!("STATS_WD_MON", "Mon").into()),
            None,
            Some(tr!("STATS_WD_WED", "Wed").into()),
            None,
            Some(tr!("STATS_WD_FRI", "Fri").into()),
            None,
            None,
        ];

        // Weekday label column sits left of the week columns.
        let mut wd_col = div()
            .flex()
            .flex_col()
            .gap(px(HEAT_GAP_PX))
            .flex_shrink_0()
            .mr(px(HEAT_GAP_PX));
        for label in &weekday_labels {
            let mut slot = div().w(px(HEAT_WD_W_PX)).h(px(HEAT_CELL_PX)).flex().items_center();
            if let Some(l) = label {
                slot = slot.child(
                    div()
                        .text_size(px(9.0))
                        .text_color(theme.text_secondary)
                        .child(l.clone()),
                );
            }
            wd_col = wd_col.child(slot);
        }

        // The window pads to whole Mon..Sun weeks: days outside the shown
        // year (previous December / next January) keep their grid slot for
        // column alignment but render invisibly.
        let year_start = NaiveDate::from_ymd_opt(self.heat_year, 1, 1).unwrap();
        let year_end = NaiveDate::from_ymd_opt(self.heat_year, 12, 31).unwrap();

        // Cells are stateful (day tooltip, 150 ms rest before it pops — fast
        // mouse travel never builds one); columns are plain layout.
        let mut grid = div().flex().flex_row().gap(px(HEAT_GAP_PX));
        for (col, week) in self.heat_weeks.iter().enumerate() {
            let mut col_div = div().flex().flex_col().gap(px(HEAT_GAP_PX));
            for (row, day) in week.days.iter().enumerate() {
                let date = self.heat_window_start
                    + ChronoDuration::days(col as i64 * 7 + row as i64);
                let base = div()
                    .w(px(HEAT_CELL_PX))
                    .h(px(HEAT_CELL_PX))
                    .rounded(px(2.0))
                    .flex_shrink_0();
                let base = if date >= year_start && date <= year_end {
                    let color = match day {
                        None | Some(0) => empty, // future days share the empty cell color
                        Some(secs) => {
                            let ratio = if self.max_day_secs > 0 {
                                *secs as f32 / self.max_day_secs as f32
                            } else {
                                1.0
                            };
                            levels[match ratio {
                                r if r > 0.75 => 3,
                                r if r > 0.5 => 2,
                                r if r > 0.25 => 1,
                                _ => 0,
                            }]
                        }
                    };
                    base.bg(color)
                } else {
                    base
                };
                let cell = if *day > Some(0) {
                    base.id(SharedString::from(format!("hm-{}", date.format("%Y%m%d"))))
                        .tooltip_show_delay(Duration::from_millis(150))
                        .tooltip(build_tooltip(SharedString::from(format!(
                            "{} · {}",
                            fmt_duration(day.unwrap_or(0)),
                            date.format("%m-%d")
                        ))))
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
                        // ‹ pages to older years (+1), › back toward today.
                        self.pager_button("stats-heat-prev", "<", theme).on_click(
                            cx.listener(|this, _, _, cx| this.shift_heat(1, cx)),
                        ),
                    )
                    .child(
                        self.pager_button("stats-heat-next", ">", theme).on_click(
                            cx.listener(|this, _, _, cx| this.shift_heat(-1, cx)),
                        ),
                    ),
            )
            .child(month_row)
            .child(
                div().flex().flex_row().flex_shrink_0().child(wd_col).child(grid),
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

    fn render_hours(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let max = self.hours_max.max(1) as f32;

        // Normalized polyline points, one per hour (x, y in 0..1 of the box).
        let points: Vec<(f32, f32)> = self
            .hours
            .iter()
            .enumerate()
            .map(|(hour, secs)| {
                (
                    (hour as f32 + 0.5) / 24.0,
                    0.93 - (*secs as f32 / max) * 0.86,
                )
            })
            .collect();

        let area_color = lerp_rgba(theme.background_secondary, theme.button_primary, 0.14);
        let line_color = theme.button_primary;
        let guide_color = lerp_rgba(line_color, theme.background_secondary, 0.6);

        // Interaction overlay: 24 transparent columns drive the crosshair
        // (written into `hover_hour`; the canvas paints a guide line + marker
        // at the data point). No column fill: a highlighted column reads as a
        // bar-chart hover, which is wrong on a line chart.
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

        // Filled area under the curve + 2 px line + crosshair, on a canvas.
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

                // Crosshair: guide line + marker dot at the hovered hour.
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

        // Hour labels under the chart.
        let mut labels = div().flex().flex_row().mt(px(2.0));
        for hour in 0..24 {
            let mut slot = div().flex_1().min_w_0().flex().justify_center();
            if hour % 6 == 0 {
                slot = slot.child(
                    div()
                        .text_size(px(9.0))
                        .text_color(theme.text_secondary)
                        .child(HOUR_LABELS[hour / 6]),
                );
            }
            labels = labels.child(slot);
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
                    .child(tr!("STATS_HOURS_TITLE", "By hour of day")),
            )
            .child(div().relative().w_full().h(px(80.0)).child(chart).child(overlay))
            .child(labels)
    }

    fn render_top(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tab_row = div().flex().flex_row().gap(px(4.0));
        for tab in [TopTab::Tracks, TopTab::Artists, TopTab::Albums] {
            let active = self.top_tab == tab;
            tab_row = tab_row.child(
                self.pill(tab.id(), tab.label(), active, theme).on_click(
                    cx.listener(move |this, _, _, cx| this.switch_tab(tab, cx)),
                ),
            );
        }
        let mut range_row = div().flex().flex_row().gap(px(4.0));
        for range in [TopRange::Week, TopRange::Month, TopRange::All] {
            let active = self.top_range == range;
            range_row = range_row.child(
                self.pill(range.id(), range.label(), active, theme).on_click(
                    cx.listener(move |this, _, _, cx| this.switch_range(range, cx)),
                ),
            );
        }

        let mut rows = div().flex().flex_col().w_full();
        // Same guaranteed-contrast tint as the hour bars: menu_item_hover is
        // invisible on this page's background in light mode.
        let row_hover = lerp_rgba(theme.background_secondary, theme.text, 0.045);
        for (i, item) in self.top_items.iter().enumerate() {
            // Stateful rows: hover styles only repaint on elements carrying an
            // id (element state drives the enter/leave notify).
            let mut row = div()
                .id(SharedString::from(format!("top-row-{}", i)))
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
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .pt(px(40.0))
                        .child(
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
                .child(self.render_hours(&theme, cx))
                .child(self.render_top(&theme, cx));
        }
        section
    }
}
