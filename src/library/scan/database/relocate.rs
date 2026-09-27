use camino::Utf8Path;
use sqlx::SqliteConnection;

use super::recompute_album_artists;
use crate::library::scan::artist_match::ArtistMatcher;

/// Move a track row after a case-only rename. Merge playlists/lyrics if the new path already has a row.
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

/// Merge the stale row at `old` into the row already at `new`: repoint playlists, drop the
/// stale row and rebuild its album's artists. Returns the repointed playlist ids.
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

    let stale_album: Option<(i64,)> = sqlx::query_as(include_str!(
        "../../../../queries/scan/get_album_id_at_track.sql"
    ))
    .bind(stale_id)
    .fetch_optional(&mut *conn)
    .await?;

    sqlx::query(include_str!("../../../../queries/scan/delete_track.sql"))
        .bind(old.as_str())
        .execute(&mut *conn)
        .await?;

    // the removed row may have been the only one crediting an artist
    if let Some((album_id,)) = stale_album {
        recompute_album_artists(conn, matcher, album_id).await?;
    }

    Ok(updated_playlists)
}
