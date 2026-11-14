use camino::Utf8Path;
use sqlx::SqliteConnection;

use super::recompute_album_artists;
use crate::library::scan::artist_match::ArtistMatcher;

/// 纯大小写改名之后移动曲目行。若新路径已有行，则合并歌单与歌词。
pub async fn relocate_track(
    conn: &mut SqliteConnection,
    matcher: &mut ArtistMatcher,
    old: &Utf8Path,
    new: &Utf8Path,
) -> anyhow::Result<Vec<i64>> {
    let Some(new_parent) = new.parent() else {
        return Ok(Vec::new());
    };

    let existing: Option<(i64,)> = sqlx::query_as(include_str!(
        "../../../../queries/scan/get_track_id_at_location.sql"
    ))
    .bind(new.as_str())
    .fetch_optional(&mut *conn)
    .await?;

    let mut updated_playlists = Vec::new();

    if let Some((target_id,)) = existing {
        updated_playlists = merge_stale_track(conn, matcher, old, target_id).await?;
    } else {
        sqlx::query(include_str!("../../../../queries/scan/relocate_track.sql"))
            .bind(new.as_str())
            .bind(new_parent.as_str())
            .bind(old.as_str())
            .execute(&mut *conn)
            .await?;
    }

    if let Some(old_parent) = old.parent() {
        sqlx::query(include_str!(
            "../../../../queries/scan/relocate_album_folder.sql"
        ))
        .bind(new_parent.as_str())
        .bind(old_parent.as_str())
        .execute(&mut *conn)
        .await?;
    }

    Ok(updated_playlists)
}

/// 把 `old` 处的过期行并入 `new` 处已有的行：重指歌单、删掉过期行，并重建其专辑的
/// 歌手链接。返回被重指的歌单 id。
async fn merge_stale_track(
    conn: &mut SqliteConnection,
    matcher: &mut ArtistMatcher,
    old: &Utf8Path,
    target_id: i64,
) -> anyhow::Result<Vec<i64>> {
    let stale: Option<(i64,)> = sqlx::query_as(include_str!(
        "../../../../queries/scan/get_track_id_at_location.sql"
    ))
    .bind(old.as_str())
    .fetch_optional(&mut *conn)
    .await?;

    let Some((stale_id,)) = stale else {
        return Ok(Vec::new());
    };

    let updated_playlists: Vec<i64> = sqlx::query_scalar::<_, i64>(include_str!(
        "../../../../queries/scan/list_playlist_ids_for_track.sql"
    ))
    .bind(old.as_str())
    .fetch_all(&mut *conn)
    .await?;

    sqlx::query(include_str!(
        "../../../../queries/scan/repoint_playlist_items.sql"
    ))
    .bind(target_id)
    .bind(stale_id)
    .execute(&mut *conn)
    .await?;

    // album_id 列可空（无专辑曲目的 standalone 行存 NULL）：按 i64 解码会在
    // NULL 处 ColumnDecode、? 上抛令整次大小写合并永久失败（每次重扫重复报错，
    // 新旧行长期并存）。与 database.rs 的 previous_album 同款按可空解码；
    // 无专辑的过期行本就没有专辑歌手链接要重建，None 直接跳过重算。
    let stale_album: Option<(Option<i64>,)> = sqlx::query_as(include_str!(
        "../../../../queries/scan/get_album_id_at_track.sql"
    ))
    .bind(stale_id)
    .fetch_optional(&mut *conn)
    .await?;
    let stale_album_id = stale_album.and_then(|(id,)| id);

    sqlx::query(include_str!("../../../../queries/scan/delete_track.sql"))
        .bind(old.as_str())
        .execute(&mut *conn)
        .await?;

    // 被删掉的那一行可能是某位歌手唯一的署名来源
    if let Some(album_id) = stale_album_id {
        recompute_album_artists(conn, matcher, album_id).await?;
    }

    Ok(updated_playlists)
}
