mod finalize;
mod processing;
mod repository;

use rustc_hash::{FxHashMap, FxHashSet};
use sqlx::SqlitePool;
use std::collections::hash_map::Entry;

use super::decode::FileArt;
use super::discover::FolderArtObservationMap;

pub use finalize::finalize_scan_art;
pub use processing::{ArtworkProcessor, FolderArtLoader};
pub(crate) use repository::get_or_create_artwork;
pub use repository::{ArtworkData, load_art_ids};

/// 按内容哈希取 artwork 行 id。`None` 表示处理失败。
pub type ArtIdCache = FxHashMap<u64, Option<i64>>;

/// 本次扫描中为每个专辑找到的最优文件夹封面哈希与来源排名。
pub type FolderArtCandidates = FxHashMap<i64, (u64, i64)>;

pub(crate) fn consider_folder_art(
    candidates: &mut FolderArtCandidates,
    album_id: i64,
    hash: u64,
    source: i64,
) {
    match candidates.entry(album_id) {
        Entry::Vacant(entry) => {
            entry.insert((hash, source));
        }
        Entry::Occupied(mut entry) => {
            let &(current_hash, current_source) = entry.get();
            if (source, hash) < (current_source, current_hash) {
                entry.insert((hash, source));
            }
        }
    }
}

/// 查找文件夹封面但不重读音频，并把文件夹已检查过的每个专辑都标记下来。
pub async fn examine_folder_art(
    pool: &SqlitePool,
    observations: &FolderArtObservationMap,
    loader: &FolderArtLoader,
    processor: &ArtworkProcessor,
    examined: &mut FxHashSet<i64>,
    candidates: &mut FolderArtCandidates,
    art_ids: &mut ArtIdCache,
) -> anyhow::Result<()> {
    if observations.is_empty() {
        return Ok(());
    }

    // 只取 disc 为 1、0 或缺失的
    let claims: Vec<(i64, String, i64)> = sqlx::query_as(include_str!(
        "../../../queries/scan/list_album_path_claims.sql"
    ))
    .fetch_all(pool)
    .await?;
    let mut claims_by_dir: FxHashMap<String, Vec<i64>> = FxHashMap::default();
    let mut claim_counts: FxHashMap<i64, usize> = FxHashMap::default();
    let mut incumbent_sources: FxHashMap<i64, i64> = FxHashMap::default();
    for (album_id, path, artwork_source) in claims {
        claims_by_dir.entry(path).or_default().push(album_id);
        *claim_counts.entry(album_id).or_default() += 1;
        // 同一专辑的每一行 album_path 都携带该专辑自身的来源
        incumbent_sources.insert(album_id, artwork_source);
    }

    let mut conn = pool.acquire().await?;
    for (directory, observation) in observations {
        let Some(album_ids) = claims_by_dir.get(directory.as_str()) else {
            continue;
        };

        // 本次扫描提交的曲目已经带上本目录的文件夹候选，因此已在 `candidates`
        // 里定下的专辑在这里只会重算出同一个哈希——跳过这次重读（弱加载句柄此时
        // 早已过期，会逼出一整次文件读取 + 解码）。与其他文件夹共享的专辑仍可能
        // 被那个文件夹的封面压过，所以那些一律重新 examine。`examined` 仍会记下
        // 这次检查，供 finalize 的「保留现有」分支使用。
        if album_ids.iter().all(|&album_id| {
            candidates.contains_key(&album_id) && claim_counts.get(&album_id) == Some(&1)
        }) {
            examined.extend(album_ids.iter().copied());
            continue;
        }

        let mut art = match observation {
            Some(candidate) => loader.load(candidate.clone()).await,
            None => None,
        };
        if let Some(folder) = art.take() {
            let mut file_art = FileArt {
                embedded: None,
                folder: Some(folder),
                representative: true,
            };
            processor.process_file_art(&mut file_art).await;
            art = file_art.folder;
        }

        for &album_id in album_ids {
            // 一旦进入 `examined`，专辑就会被送进 finalize（每次扫描、每个专辑至多
            // 五条语句）。所以只在本次确实可能改变答案的地方标记：此刻找到了封面，
            // 或者文件夹在位者可能随其封面文件一起消失。没有封面文件目录里的内嵌
            // 在位者（source 0）只能把本次扫描已证明未变的多数派重投一遍——而对
            // 迁移来的历史行（专辑有封面、曲目没有 `art_hash`），那遍重投的答案是
            // 「没有封面」并会清空专辑，因此跳过是同一个空操作里更安全的那一半。
            if art.is_none() && incumbent_sources.get(&album_id).copied().unwrap_or(0) == 0 {
                continue;
            }
            examined.insert(album_id);

            if let Some(art) = &art {
                let hash = art.hash;

                if let Entry::Vacant(e) = art_ids.entry(hash) {
                    let data = art.processed.as_deref().map(ArtworkData::Processed);
                    let id = get_or_create_artwork(&mut conn, hash as i64, data).await;
                    e.insert(id);
                }

                let source = art.source.db_value();
                consider_folder_art(candidates, album_id, hash, source);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "artwork/tests.rs"]
mod tests;
