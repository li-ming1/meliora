use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;
use sqlx::SqliteConnection;
use tracing::warn;

use super::{albums::bind_release_date, artists::TrackArtistRow, artists::encode_artist_list};
use crate::{
    library::scan::fs_case::paths_equal,
    media::{SUPPORTED_EXTENSIONS, metadata::Metadata},
};

/// 每次扫描的专辑路径缓存的键：`(album id, disc 号)`。
pub type AlbumPathCacheKey = (i64, i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackWriteOutcome {
    Written,
    /// 跳过——专辑在另一个文件夹里已有一份磁盘上仍然存在的有效副本。
    SkippedDuplicateFolder,
}

/// 专辑的文件夹认领下面是否仍有曲目行支撑。
async fn album_path_still_populated(
    conn: &mut SqliteConnection,
    album_id: i64,
    disc_num: i64,
    claimed_folder: &Utf8Path,
) -> anyhow::Result<bool> {
    let folders: Vec<(String,)> = sqlx::query_as(include_str!(
        "../../../../queries/scan/album_path_still_populated.sql"
    ))
    .bind(album_id)
    .bind(disc_num)
    .fetch_all(&mut *conn)
    .await?;
    // 按卷的大小写规则匹配，而不是按 SQL 的字节相等
    Ok(folders
        .iter()
        .any(|(folder,)| paths_equal(Utf8Path::new(folder), claimed_folder)))
}

async fn repair_album_path(
    conn: &mut SqliteConnection,
    album_id: i64,
    disc_num: i64,
    new_folder: &Utf8Path,
) -> anyhow::Result<()> {
    sqlx::query(include_str!(
        "../../../../queries/scan/update_album_path.sql"
    ))
    .bind(album_id)
    .bind(disc_num)
    .bind(new_folder.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(super) async fn upsert_lyrics(
    conn: &mut SqliteConnection,
    track_id: i64,
    content: &str,
) -> anyhow::Result<()> {
    sqlx::query(include_str!("../../../../queries/scan/upsert_lyrics.sql"))
        .bind(track_id)
        .bind(content)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(super) async fn delete_lyrics(
    conn: &mut SqliteConnection,
    track_id: i64,
) -> anyhow::Result<()> {
    sqlx::query(include_str!("../../../../queries/scan/delete_lyrics.sql"))
        .bind(track_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// 判定 claimed 文件夹在磁盘上是否还能拿出一层之内的受支持音频文件。
///
/// 只 stat 一层不深扫：claimed 文件夹本来就是曲目行的直接父目录，专辑自己的
/// 文件应当就在这一层。语义：
/// - 目录 stat 不到（NotFound）或位置已被文件占据 → `false`（文件夹已不在）；
/// - 目录还在但一层之内没有任何受支持扩展名的条目 → `false`；
/// - 其余任何 IO 失败（权限、读目录抖动、读不出条目名）→ `true`，按「目录仍在」
///   保守处理，不因探测抖动把真重复副本误判成陈旧认领。
///
/// 同步 IO 只发生在 mismatch 路径（罕见），成本可接受。
fn claimed_folder_holds_audio(claimed: &Utf8Path) -> bool {
    match std::fs::metadata(claimed) {
        // 目录不存在 = 认领指向的位置已消失，行是陈旧的
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        // 其余 stat 失败（权限、挂载抖动等）按「目录仍在」保守处理
        Err(_) => return true,
        // 位置被文件占据 = 原文件夹已不在
        Ok(meta) if !meta.is_dir() => return false,
        Ok(_) => {}
    }
    let Ok(entries) = std::fs::read_dir(claimed) else {
        // 读目录失败同样保守处理
        return true;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return true;
        };
        // file_name() 返回所有权 OsString，先绑定再借用其字符串视图，
        // 否则临时值在本语句末即释放。
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return true;
        };
        let is_audio = Utf8Path::new(name).extension().is_some_and(|extension| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        });
        if is_audio {
            return true;
        }
    }
    false
}

/// 文件夹不匹配时：旧文件夹在磁盘上已拿不出音频文件则更新认领，否则按重复副本
/// 拒绝这一条。
///
/// 「已拿不出」以磁盘为准而不是 DB 行：`album_path_still_populated` 只能证明还有
/// 行指向旧文件夹，而从未监视位置移入的专辑其源行没有任何事件去清（watcher 只对
/// 目标目录发 RescanPaths，full 清理要等下一次全量扫描），按行拒绝会把拒绝写进
/// 扫描记录，目标位置的文件从此被所有扫描跳过——专辑静默永久丢失。行存在时用
/// [`claimed_folder_holds_audio`] 探测磁盘，探测说旧文件夹仍在且有音频文件才维持
/// 真重复副本的拒绝。
async fn handle_folder_mismatch(
    conn: &mut SqliteConnection,
    album_path_cache: &mut FxHashMap<AlbumPathCacheKey, Utf8PathBuf>,
    album_path_key: AlbumPathCacheKey,
    claimed: &Utf8Path,
    parent: &Utf8Path,
) -> anyhow::Result<bool> {
    let (album_id, disc_num) = album_path_key;
    if album_path_still_populated(conn, album_id, disc_num, claimed).await? {
        if claimed_folder_holds_audio(claimed) {
            warn!(
                "Rejecting track in {:?}: album id {} disc {} is claimed by {:?} (duplicate copy of the album?)",
                parent, album_id, disc_num, claimed
            );
            return Ok(false);
        }
        warn!(
            "Album id {} disc {} claim on {:?} is stale (folder no longer holds audio files); re-claiming to {:?}",
            album_id, disc_num, claimed, parent
        );
    }

    repair_album_path(conn, album_id, disc_num, parent).await?;
    // 否则缓存会在本次扫描剩余时间里一直供出旧路径
    album_path_cache.insert(album_path_key, parent.to_path_buf());
    Ok(true)
}

/// 插入一条扫描到的曲目行，返回新的曲目 id。
///
/// 先解析专辑-文件夹认领：文件夹不匹配要么修好这个认领（旧文件夹在磁盘上已
/// 拿不出音频文件），要么把该曲目当作专辑的重复副本拒绝掉，后者给出 `Ok(None)`。
pub(super) async fn insert_track(
    conn: &mut SqliteConnection,
    metadata: &Metadata,
    album_id: Option<i64>,
    art_hash: Option<i64>,
    path: &Utf8Path,
    length: u64,
    album_path_cache: &mut FxHashMap<AlbumPathCacheKey, Utf8PathBuf>,
) -> anyhow::Result<Option<(i64, TrackArtistRow)>> {
    // 光秃秃的文件名（没有父目录）退化成空专辑路径，
    // 而不是让整次扫描中止。
    let parent = path.parent().unwrap_or_else(|| Utf8Path::new(""));

    if let Some(album_id) = album_id {
        let disc_num = metadata.disc_current.map(|v| v as i64).unwrap_or(-1);
        let album_path_key = (album_id, disc_num);

        if album_path_cache.get(&album_path_key).is_none() {
            let recorded_folder: Option<(String,)> =
                sqlx::query_as(include_str!("../../../../queries/scan/get_album_path.sql"))
                    .bind(album_id)
                    .bind(disc_num)
                    .fetch_optional(&mut *conn)
                    .await?;

            let resolved = match recorded_folder {
                Some((folder,)) => Utf8PathBuf::from(folder),
                None => {
                    sqlx::query(include_str!(
                        "../../../../queries/scan/create_album_path.sql"
                    ))
                    .bind(album_id)
                    .bind(parent.as_str())
                    .bind(disc_num)
                    .execute(&mut *conn)
                    .await?;
                    parent.to_path_buf()
                }
            };
            album_path_cache.insert(album_path_key, resolved);
        }

        let claimed = album_path_cache
            .get(&album_path_key)
            .expect("album path cache populated above");
        if !paths_equal(claimed, parent) {
            let claimed = claimed.clone();
            if !handle_folder_mismatch(conn, album_path_cache, album_path_key, &claimed, parent)
                .await?
            {
                return Ok(None);
            }
        }
    }

    let name = metadata
        .name
        .as_deref()
        .or_else(|| path.file_name())
        .ok_or_else(|| anyhow::anyhow!("failed to retrieve filename"))?;

    let (release_date, date_precision) = bind_release_date(metadata);
    let artists = encode_artist_list(&metadata.artists);
    let album_artist_keys = encode_artist_list(&metadata.album_artist_keys);

    // create_track.sql 的 RETURNING 行型：track_id + 四列 artist 直通
    // （pending_tracks 携带直通优化的载体，字段序与 TrackArtistRow 一一对应）。
    type CreateTrackReturningRow = (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let result: Result<CreateTrackReturningRow, sqlx::Error> =
        sqlx::query_as(include_str!("../../../../queries/scan/create_track.sql"))
            .bind(name)
            .bind(name)
            .bind(album_id)
            .bind(metadata.track_current.map(|x| x as i32))
            .bind(metadata.disc_current.map(|x| x as i32))
            .bind(length as i32)
            .bind(path.as_str())
            .bind(&metadata.genre)
            .bind(&metadata.artist)
            .bind(parent.as_str())
            .bind(metadata.replaygain_track_gain)
            .bind(metadata.replaygain_track_peak)
            .bind(metadata.replaygain_album_gain)
            .bind(metadata.replaygain_album_peak)
            .bind(&metadata.disc_subtitle)
            .bind(artists)
            .bind(&metadata.artist_sort)
            .bind(album_artist_keys)
            .bind(art_hash)
            .bind(release_date)
            .bind(date_precision)
            .fetch_one(&mut *conn)
            .await;

    match result {
        // RETURNING 连 artist 四列一起回：待处理集携带数据库亲手写下的行
        // 直通 flush，免掉逐行回读（pending_tracks 携带直通优化）
        Ok((track_id, artists, artist_sort, album_artist_keys, artist_names)) => Ok(Some((
            track_id,
            TrackArtistRow {
                artists,
                artist_sort,
                album_artist_keys,
                artist_names,
            },
        ))),
        Err(sqlx::Error::RowNotFound) => Err(anyhow::anyhow!("create_track returned no row")),
        Err(e) => Err(e.into()),
    }
}
