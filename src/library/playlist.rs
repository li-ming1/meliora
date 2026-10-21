use std::{ffi::OsStr, path::PathBuf};

use anyhow::Context as _;
use cntp_i18n::tr;
use futures::{StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use gpui::{App, AppContext, Entity, EventEmitter, PathPromptOptions};
use sqlx::{Sqlite, SqlitePool};
use tokio::{fs::File, io::BufWriter};
use tracing::{Instrument as _, debug_span, error, info, warn};

use crate::library::Pool;

// 歌单变更通知。这些过去住在 `ui::models` 里（审计：`library::scan` 与本模块为了发出
// 它们不得不反向 import UI 层）；它们属于歌单域状态，所以现在搬到这里。tracker 实体本身
// 仍由 UI 层创建（`build_models`）并由 `Models.playlist_tracker` 持有；调用方把它传进
// `import_playlist`（与 `scan::control::start_broadcast` 同一模式），从而本模块保持对
// `crate::ui` 零 import。`ui::models` 会把这两个类型再导出，供既有的
// `ui::models::{PlaylistEvent, PlaylistInfoTransfer}` 使用方继续取用。
pub struct PlaylistInfoTransfer;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlaylistEvent {
    PlaylistUpdated(i64),
    PlaylistDeleted(i64),
}

impl EventEmitter<PlaylistEvent> for PlaylistInfoTransfer {}

/// 广播"某个歌单变了"，让所有展示它的地方回读。十四个站点写的都是同一条
/// `update + emit`（都不在渲染路径上：菜单回调、拖放、spawn 完成处、点赞/取消
/// 点赞的四处收尾），而这条广播的唯一出口本来就该是 `PlaylistInfoTransfer`
/// 这一个实体。
/// 泛型过 `AppContext`：那些站点里多数在 `cx.spawn` 的异步块内，手里的 `cx`
/// 是 `AsyncApp` 而不是 `App`。
pub fn announce_playlist_update(
    tracker: &Entity<PlaylistInfoTransfer>,
    cx: &mut impl AppContext,
    playlist_id: i64,
) {
    tracker.update(cx, |_, cx| {
        cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
    });
}

#[cfg(windows)]
const LINE_ENDING: &str = "\r\n";
#[cfg(not(windows))]
const LINE_ENDING: &str = "\n";

#[derive(sqlx::FromRow)]
struct PlaylistEntry {
    location: String,
    duration: u32,
    track_artist_names: String,
    artist_name: String,
    track_title: String,
    album_title: String,
}

async fn write_m3u(mut w: BufWriter<File>, pool: &SqlitePool, pl_id: i64) -> anyhow::Result<()> {
    use std::io::Write as _;
    use tokio::io::AsyncWriteExt as _;

    w.write_all(b"#EXTM3U").await?;

    {
        let query = include_str!("../../queries/playlist/list_tracks_for_export.sql");
        let mut entries = sqlx::query_as(query).bind(pl_id).fetch(pool);
        let mut buf = vec![];
        while let Some(PlaylistEntry {
            location,
            duration,
            track_artist_names,
            artist_name,
            track_title,
            album_title,
        }) = entries.try_next().await?
        {
            write!(
                &mut buf,
                "{LINE_ENDING}\
                #EXTINF:{duration},{track_artist_names} - {track_title}{LINE_ENDING}\
                #EXTALB:{album_title}{LINE_ENDING}\
                #EXTART:{artist_name}{LINE_ENDING}\
                {location}{LINE_ENDING}",
            )?;

            w.write_all(&buf).await?;
            buf.clear();
        }
    }

    w.shutdown().await?;
    Ok(())
}

pub fn export_playlist(cx: &App, pl_id: i64, playlist_name: &str) -> anyhow::Result<()> {
    let path_future = cx.prompt_for_new_path(
        directories::UserDirs::new()
            .context("Failed to get user directories")?
            .document_dir()
            .context("Failed to get documents directory")?,
        Some(&format!("{playlist_name}.m3u8")),
    );
    let pool = cx.global::<Pool>().0.clone();

    crate::RUNTIME.spawn(async move {
        let path = match path_future.err_into().await.flatten() {
            Ok(Some(path)) => path,
            Ok(None) => return info!("Playlist export cancelled by user"),
            Err(err) => return error!(?err, "Failed to prompt for path: {err}"),
        };

        if let Err(err) = File::create(&path)
            .err_into()
            .map_ok(BufWriter::new)
            .and_then(|f| write_m3u(f, &pool, pl_id))
            .instrument(debug_span!("export_playlist", pl_id, path = %path.display()))
            .await
        {
            error!(?err, "Failed writing playlist to {}: {err}", path.display());
        }
    });

    Ok(())
}

#[derive(Debug, Default)]
struct M3UEntry {
    duration: Option<u32>,
    track_artist_names: Option<String>,
    track_title: Option<String>,
    album_title: Option<String>,
    artist_name: Option<String>,
    location: PathBuf,
}

fn parse_m3u(file: File) -> impl futures::Stream<Item = anyhow::Result<M3UEntry>> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio_stream::wrappers::LinesStream;

    let lines = LinesStream::new(BufReader::new(file).lines()).enumerate();
    futures::stream::try_unfold(lines, async |mut lines| {
        let mut current_entry = M3UEntry::default();
        while let Some((line, res)) = lines.next().await {
            let txt = match res {
                Ok(txt) => txt,
                // 单行不是合法 UTF-8（GBK/Latin-1 歌单里的中文路径很常见）：
                // 丢掉这一条继续读后面的行。`lines()` 已消费掉坏行，下一次
                // next() 从下一行继续；若在此终止，一条坏路径就会让歌单在
                // 该行之后被整体截断（而导入前刚刚清空过目标歌单）。
                // 其余 IO 错误仍结束整条流——继续读只会重复同一个错误。
                Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                    error!(%line, ?err, "M3U line is not valid UTF-8; skipping it");
                    current_entry = M3UEntry::default();
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            if let Some(line) = txt.strip_prefix("#EXTINF:") {
                let Some((dur, info)) = line.split_once(',') else {
                    continue;
                };

                match dur.parse() {
                    Ok(dur) => current_entry.duration = Some(dur),
                    Err(err) => warn!(%line, ?err, "Failed to parse track duration: {err}"),
                }

                if let Some((artist, title)) = info.split_once(['-', ':', '\u{2013}']) {
                    current_entry.track_artist_names = Some(artist.trim().into());
                    current_entry.track_title = Some(title.trim().into());
                } else {
                    current_entry.track_title = Some(info.trim().into());
                }
            } else if let Some(album_title) = txt.strip_prefix("#EXTALB:") {
                current_entry.album_title = Some(album_title.into());
            } else if let Some(artist_name) = txt.strip_prefix("#EXTART:") {
                current_entry.artist_name = Some(artist_name.into());
            } else if !txt.starts_with('#') && !txt.is_empty() {
                current_entry.location = txt.into();
                tracing::debug!("Parsed track: {current_entry:?}");
                return Ok(Some((current_entry, lines)));
            } else {
                tracing::debug!(%line, "Ignoring line: '{txt}'");
            }
        }

        Ok(None)
    })
}

/// 转义 LIKE 模式里的 `\`、`%`、`_`（配合 SQL 的 `ESCAPE '\'`）：文件名里的
/// 下划线很常见，不转义会把 `a_b` 匹配成 `axb`，把曲目导入到错误的位置。
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// M3U 单条目 → 库内 track id 的两阶段查找。阶段一只做索引化精确路径匹配
/// （`track.location` 上有 UNIQUE 隐式索引，EXPLAIN 为 SEARCH … USING COVERING
/// INDEX），只有没命中的少数条目才进阶段二的模糊匹配。原实现把两支放在同一个
/// 顶层 OR 里，SQLite 因此整体放弃索引 → `SCAN t`，即每条 M3U 曲目一次全表扫描：
/// 按 100k 实测标尺（`list_all_tracks` 283ms）2000 行的导入要数百秒，且占满
/// 3 条连接池，饿住行预取那侧 UI 线程的 `block_on`
/// （见 queries/playlist/lookup_track_by_location.sql）。
/// 这段本体从 `import_playlist` 的流式映射里原样搬来，一条 SQL 都没改。
async fn lookup_m3u_track(pool: &SqlitePool, result: anyhow::Result<M3UEntry>) -> Option<i64> {
    let entry = match result {
        Ok(entry) => entry,
        Err(err) => {
            error!(?err, "Error parsing M3U entry: {err}");
            return None;
        }
    };
    let location = entry.location.clone();
    let exact_query = include_str!("../../queries/playlist/lookup_track_by_location.sql");
    let exact_hit = sqlx::query_scalar::<Sqlite, i64>(exact_query)
        .bind(entry.location.to_string_lossy().into_owned())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    if let Some(id) = exact_hit {
        return Some(id);
    }

    // 精确匹配本就是原 OR 的第一支，所以命中集严格不劣化。
    let lookup_query = include_str!("../../queries/playlist/lookup_track.sql");
    match sqlx::query_scalar::<Sqlite, i64>(lookup_query)
        .bind(entry.location.to_string_lossy().into_owned())
        .bind(entry.track_title)
        .bind(entry.artist_name)
        .bind(entry.album_title)
        .bind(entry.track_artist_names)
        .bind(entry.duration)
        .bind(format!(
            "%{}%",
            escape_like(
                entry
                    .location
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
            )
        ))
        .fetch_one(pool)
        .await
    {
        Ok(id) => Some(id),
        Err(err) => {
            warn!(
                ?err,
                "Failed to find track for '{}': {err}",
                location.display()
            );
            None
        }
    }
}

/// 从用户选中的 M3U 文件把曲目导入 `playlist_id`。歌单 tracker 由 UI 调用方传入
/// （在紧邻调用之前的那条语句里从全局 `Models` 读出，正是本函数过去自己去取它的同一个
/// 执行点），因此本模块不引入任何 `crate::ui` 依赖。
pub fn import_playlist(cx: &App, playlist_id: i64, playlist_tracker: Entity<PlaylistInfoTransfer>) {
    let path_future = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(tr!("SELECT_M3U", "Select a M3U file...").into()),
    });

    let pool = cx.global::<Pool>().0.clone();

    cx.spawn(async move |cx| {
        let task = crate::RUNTIME.spawn(async move {
            let Some(path) = path_future.await??.and_then(|v| v.into_iter().next()) else {
                info!("Playlist import cancelled by user");
                return anyhow::Ok(false);
            };

            let span = tracing::debug_span!("import_playlist", playlist_id, path = %path.display());
            let mut ids: Vec<i64> = parse_m3u(File::open(path).await?)
                .map(|result| {
                    let pool = pool.clone();
                    async move { lookup_m3u_track(&pool, result).await }
                })
                .buffered(8)
                .filter_map(std::future::ready)
                .collect()
                .instrument(debug_span!(parent: &span, "lookup_tracks"))
                .await;

            if ids.is_empty() {
                warn!("M3U import for playlist {playlist_id} resolved to no tracks; leaving playlist untouched");
                return anyhow::Ok(false);
            }

            // 保序去重：同一曲目在 M3U 里出现两次很常见（列两遍，或相对/绝对路径解析到
            // 同一 track），而 `playlist_item(playlist_id, track_id)` 是唯一索引、插入用
            // 的是普通 INSERT——重复项会让整批导入在**所有查询都跑完之后**因约束冲突
            // 回滚，用户看到的就是"导入失败"。
            let mut seen = rustc_hash::FxHashSet::default();
            ids.retain(|id| seen.insert(*id));

            let mut tx = pool.begin().await?;

            let reset_query = include_str!("../../queries/playlist/empty_playlist.sql");
            sqlx::query(reset_query)
                .bind(playlist_id)
                .execute(&mut *tx)
                .await?;

            let insert_query = include_str!("../../queries/playlist/add_track.sql");
            for track_id in ids {
                sqlx::query(insert_query)
                    .bind(playlist_id)
                    .bind(track_id)
                    .execute(&mut *tx)
                    .instrument(debug_span!(parent: &span, "insert_track", track_id))
                    .await?;
            }

            tx.commit().await?;

            anyhow::Ok(true)
        });

        let updated = match task.err_into().await.flatten() {
            Ok(updated) => updated,
            Err(err) => {
                error!(?err, "Failed to import playlist");
                return;
            }
        };

        if !updated {
            return;
        }

        announce_playlist_update(&playlist_tracker, cx, playlist_id);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_pool, insert_metadata, track_metadata};
    use sqlx::Row as _;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(escape_like("a_b%c\\d"), "a\\_b\\%c\\\\d");
        assert_eq!(escape_like("plain"), "plain");
    }

    const EXACT_SQL: &str = include_str!("../../queries/playlist/lookup_track_by_location.sql");
    // sqlx 0.9 禁止运行期拼接的 SQL 串，故在编译期把 EXPLAIN 前缀与查询文件拼成静态串
    // （`concat!` 接受 `include_str!`）。
    const EXPLAIN_EXACT: &str = concat!(
        "EXPLAIN QUERY PLAN ",
        include_str!("../../queries/playlist/lookup_track_by_location.sql")
    );
    const FUZZY_SQL: &str = include_str!("../../queries/playlist/lookup_track.sql");

    async fn seed_one(pool: &SqlitePool, path: &camino::Utf8Path) -> i64 {
        let mut conn = pool.acquire().await.unwrap();
        insert_metadata(
            &mut conn,
            &track_metadata("Album A", "Artist B", "Song C", 3),
            path,
        )
        .await
        .unwrap();
        drop(conn);
        sqlx::query_scalar::<_, i64>("SELECT id FROM track WHERE location = ?")
            .bind(path.as_str())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// 非 UTF-8 行只丢自身：GBK/Latin-1 歌单里的中文路径不能让其后条目被截断，
    /// 也不能把坏行之前那条的元数据粘到下一首上。
    #[tokio::test]
    async fn parse_m3u_skips_non_utf8_lines_instead_of_truncating() {
        use futures::StreamExt as _;

        let (dir, _pool) = create_test_pool("meliora-m3u-non-utf8").await;
        let path = dir.join("list.m3u");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"#EXTINF:1,A - One\n/music/one.flac\n");
        bytes.extend_from_slice(b"#EXTINF:2,B - Two\n/music/");
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        bytes.extend_from_slice(b".flac\n");
        bytes.extend_from_slice(b"#EXTINF:3,C - Three\n/music/three.flac\n");
        std::fs::write(&path, bytes).unwrap();

        let entries: Vec<M3UEntry> = parse_m3u(File::open(&path).await.unwrap())
            .filter_map(|result| std::future::ready(result.ok()))
            .collect()
            .await;

        let locations: Vec<String> = entries
            .iter()
            .map(|entry| entry.location.display().to_string())
            .collect();
        assert_eq!(locations, vec!["/music/one.flac", "/music/three.flac"]);
        assert_eq!(
            entries[1].track_title.as_deref(),
            Some("Three"),
            "坏行之后的条目不应继承前一条的标题"
        );
    }

    /// 两阶段解析的等价性与计划形状回归（2026-10-17 修 `SCAN t` 时钉下）：
    /// 阶段一必须命中索引、且与原模糊查询在精确路径命中时给出同一个 id；
    /// 路径变了的时候原模糊分支仍要能靠标题找回。
    #[tokio::test]
    async fn m3u_lookup_prefers_indexed_exact_location() {
        let (dir, pool) = create_test_pool("meliora-m3u-lookup").await;
        let path = dir.utf8_join("song.flac");
        let id = seed_one(&pool, &path).await;

        let exact: Option<i64> = sqlx::query_scalar::<Sqlite, i64>(EXACT_SQL)
            .bind(path.as_str())
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(exact, Some(id), "阶段一必须解析出灌入的曲目");

        let stem = format!("%{}%", path.file_stem().unwrap());
        let fuzzy_same: Option<i64> = sqlx::query_scalar::<Sqlite, i64>(FUZZY_SQL)
            .bind(path.as_str())
            .bind(None::<String>)
            .bind(None::<String>)
            .bind(None::<String>)
            .bind(None::<String>)
            .bind(None::<u32>)
            .bind(&stem)
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(exact, fuzzy_same, "精确命中时两阶段与原查询必须同解");

        // 路径不同（M3U 记录的是旧位置）：阶段一未命中，模糊分支按标题找回。
        let fuzzy_moved: Option<i64> = sqlx::query_scalar::<Sqlite, i64>(FUZZY_SQL)
            .bind("/nowhere/song.flac")
            .bind("Song C")
            .bind(None::<String>)
            .bind(None::<String>)
            .bind(None::<String>)
            .bind(None::<u32>)
            .bind("%song%")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(fuzzy_moved, Some(id), "模糊回退必须仍能解析同一曲目");

        let rows = sqlx::query(EXPLAIN_EXACT).fetch_all(&pool).await.unwrap();
        let detail: Vec<String> = rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("detail").ok())
            .collect();
        let joined = detail.join(" | ");
        assert!(
            joined.contains("USING") && !joined.contains("SCAN"),
            "阶段一必须走索引查找，实际计划：{joined}"
        );
    }

    /// `playlist_item(playlist_id, track_id)` 是唯一索引而导入用普通 INSERT，
    /// 所以导入侧的保序去重不是可选优化：少了它，含重复曲目的 M3U 会在全部
    /// 查询跑完之后因约束冲突让整批事务回滚。
    #[tokio::test]
    async fn duplicate_track_ids_would_violate_the_playlist_unique_index() {
        let (dir, pool) = create_test_pool("meliora-m3u-dup").await;
        let path = dir.utf8_join("song.flac");
        let id = seed_one(&pool, &path).await;

        let mut conn = pool.acquire().await.unwrap();
        let first = sqlx::query(include_str!("../../queries/playlist/add_track.sql"))
            .bind(1_i64)
            .bind(id)
            .execute(&mut *conn)
            .await;
        assert!(first.is_ok(), "首次插入应成功");

        let second = sqlx::query(include_str!("../../queries/playlist/add_track.sql"))
            .bind(1_i64)
            .bind(id)
            .execute(&mut *conn)
            .await;
        assert!(
            second.is_err(),
            "重复曲目必须撞唯一索引——证明导入侧去重是必需项"
        );
    }
}
