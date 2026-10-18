use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Once,
        atomic::{AtomicU64, Ordering},
    },
};

use camino::{Utf8Path, Utf8PathBuf};
use sqlx::{SqliteConnection, SqlitePool};

use crate::{
    library::{
        db,
        scan::{
            artist_match::ArtistMatcher,
            database::{WriteCaches, flush_album_artists, flush_track_artists, update_metadata},
            decode::FileArt,
        },
    },
    media::{lofty, lookup_table, metadata::Metadata, symphonia},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) mod alloc_guard {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNTING: Cell<bool> = const { Cell::new(false) };
        static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) struct CountingAllocator;

    fn record() {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
    }

    /// 在挂起分配计数的情况下运行 `f`。
    ///
    /// 这是为了阻止其他库的分配被算进 Meliora 自己的解码/转换路径。
    pub(crate) fn exempt<T>(f: impl FnOnce() -> T) -> T {
        let was_counting = COUNTING.with(Cell::get);
        COUNTING.with(|counting| counting.set(false));
        let result = f();
        COUNTING.with(|counting| counting.set(was_counting));
        result
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record();
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // 这些暂时不关心
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    pub(crate) fn count_allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
        ALLOCATIONS.with(|count| count.set(0));
        COUNTING.with(|counting| counting.set(true));
        let result = f();
        COUNTING.with(|counting| counting.set(false));
        (result, ALLOCATIONS.with(Cell::get))
    }
}

pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(prefix: &str) -> Self {
        // 每个测试进程唯一：崩溃/被杀留下的残留目录不能带着旧内容复用（Windows
        // 可能拒绝删除，因为 SQLite 句柄尚未关闭）
        static RUN_ID: LazyLock<u32> = LazyLock::new(rand::random);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{id}", *RUN_ID));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub(crate) fn utf8_path(&self) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(self.path.clone()).unwrap()
    }

    pub(crate) fn utf8_join(&self, name: &str) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(self.path.join(name)).unwrap()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 每个测试进程只注册一次内置媒体 provider。
pub(crate) fn register_test_media_providers() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        lookup_table::register_providers(vec![
            Box::new(lofty::LoftyProvider),
            Box::new(symphonia::SymphoniaProvider),
        ]);
    });
}

pub(crate) async fn create_test_pool(prefix: &str) -> (TestDir, SqlitePool) {
    let dir = TestDir::new(prefix);
    let pool = db::create_pool(dir.join("library.db")).await.unwrap();
    (dir, pool)
}

/// 在一个事务里用原始 SQL 造一个合成音乐库：`artists` 个艺术家、`albums` 张专辑
/// （每张挂到同号艺术家）、`albums * tracks_per_album` 首曲目（每首挂到所属专辑的
/// 艺术家）。形状与真实扫描写入足够接近，可用于读路径基准（1 万曲 ≈ 200 专辑 ×
/// 50）；写路径本身由扫描测试和 10 万的 `scale_probe` 覆盖。
pub(crate) async fn seed_library(
    pool: &SqlitePool,
    albums: usize,
    tracks_per_album: usize,
    artists: usize,
) {
    let mut tx = pool.begin().await.unwrap();
    for artist in 0..artists {
        sqlx::query("INSERT INTO artist (id, name, name_sortable) VALUES ($1, $2, $3)")
            .bind(artist as i64)
            .bind(format!("artist-{artist:04}"))
            .bind(format!("artist-{artist:04}"))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for album in 0..albums {
        sqlx::query("INSERT INTO album (id, title, title_sortable) VALUES ($1, $2, $3)")
            .bind(album as i64)
            .bind(format!("album-{album:04}"))
            .bind(format!("album-{album:04}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO album_artist (album_id, artist_id) VALUES ($1, $2)")
            .bind(album as i64)
            .bind(album.min(artists - 1) as i64)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let mut id = 0i64;
    for album in 0..albums {
        for track in 0..tracks_per_album {
            sqlx::query(
                "INSERT INTO track (id, title, title_sortable, album_id, track_number, duration, location) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(format!("track-{track:03}"))
            .bind(format!("track-{track:03}"))
            .bind(album as i64)
            .bind(track as i64)
            .bind(180)
            .bind(format!(r"C:\bench\album-{album:04}\track-{track:03}.flac"))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query("INSERT INTO track_artist (track_id, artist_id) VALUES ($1, $2)")
                .bind(id)
                .bind(album.min(artists - 1) as i64)
                .execute(&mut *tx)
                .await
                .unwrap();
            id += 1;
        }
    }
    tx.commit().await.unwrap();
}

/// 计时前的免计时预热轮次；把首次调用的开销（页缓存填充、lazy static）挡在样本
/// 之外。
pub(crate) const BENCH_WARMUP: usize = 50;
/// 每个探针的计时轮次；结论性数字取最小值。
pub(crate) const BENCH_ITERATIONS: usize = 400;

/// 先跑 `BENCH_WARMUP` 轮免计时再计时 `f`，并以微秒打印
/// min/p50/p90/p99/max/mean 摘要——逐行基准读的正是这种微查询格式。
pub(crate) fn bench_summarize(name: &str, mut f: impl FnMut()) {
    for _ in 0..BENCH_WARMUP {
        f();
    }
    let mut samples = Vec::with_capacity(BENCH_ITERATIONS);
    for _ in 0..BENCH_ITERATIONS {
        let start = std::time::Instant::now();
        f();
        samples.push(start.elapsed().as_nanos() as u64 / 1_000);
    }
    samples.sort_unstable();
    let mean = samples.iter().sum::<u64>() / samples.len() as u64;
    println!(
        "{name:<28} min={:>6}µs  p50={:>6}µs  p90={:>6}µs  p99={:>6}µs  max={:>6}µs  mean={:>6}µs",
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() * 9 / 10],
        samples[samples.len() * 99 / 100],
        samples[samples.len() - 1],
        mean,
    );
}

/// 与 [`bench_summarize`] 相同，但以毫秒为单位并显式指定轮次——用于单次运行要
/// 几十 ms 的探针（图像管线、索引构建），那里默认 400 轮要跑几分钟。轮次少于 10
/// 时百分位退化为 min/mean。
pub(crate) fn bench_summarize_n(name: &str, warmup: usize, runs: usize, mut f: impl FnMut()) {
    for _ in 0..warmup {
        f();
    }
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = std::time::Instant::now();
        f();
        samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = samples.iter().sum::<f64>() / runs as f64;
    if runs >= 10 {
        println!(
            "{name:<34} min={:>8.1}ms  p50={:>8.1}ms  p90={:>8.1}ms  p99={:>8.1}ms  mean={:>8.1}ms  ({runs} runs)",
            samples[0],
            samples[runs / 2],
            samples[runs * 9 / 10],
            samples[runs.min(100) - 1],
            mean,
        );
    } else {
        println!(
            "{name:<34} min={:>8.1}ms  mean={:>8.1}ms  ({runs} runs)",
            samples[0], mean,
        );
    }
}

pub(crate) fn track_metadata(album: &str, artist: &str, title: &str, track: u64) -> Metadata {
    Metadata {
        name: Some(title.to_string()),
        artist: Some(artist.to_string()),
        album_artist: Some(artist.to_string()),
        album: Some(album.to_string()),
        track_current: Some(track),
        disc_current: Some(1),
        ..Metadata::default()
    }
}

pub(crate) async fn insert_metadata(
    conn: &mut SqliteConnection,
    metadata: &Metadata,
    path: &Utf8Path,
) -> anyhow::Result<()> {
    let mut matcher = ArtistMatcher::new();
    let mut caches = WriteCaches::default();
    update_metadata(
        conn,
        metadata,
        path,
        100,
        &FileArt::default(),
        false,
        &mut caches,
    )
    .await?;
    flush_album_artists(conn, &mut matcher, &mut caches.pending_albums).await?;
    flush_track_artists(conn, &mut matcher, &mut caches.pending_tracks).await?;
    Ok(())
}

pub(crate) async fn add_track_to_playlist(
    pool: &SqlitePool,
    track_path: &Utf8Path,
    playlist_name: &str,
) -> i64 {
    let playlist_id = db::create_playlist(pool, playlist_name).await.unwrap();
    let (track_id,): (i64,) = sqlx::query_as("SELECT id FROM track WHERE location = $1")
        .bind(track_path.as_str())
        .fetch_one(pool)
        .await
        .unwrap();
    db::add_playlist_item(pool, playlist_id, track_id)
        .await
        .unwrap();
    playlist_id
}

/// 剥掉 `std::fs::canonicalize` 在 Windows 上产生的 verbatim（`\\?\`）前缀，
/// 好让结果与普通写法相等。在其他平台、以及本身无前缀的路径上是空操作。
pub(crate) fn strip_verbatim_prefix(path: Utf8PathBuf) -> Utf8PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path.as_str().strip_prefix(r"\\?\UNC\") {
        return Utf8PathBuf::from(format!(r"\\{rest}"));
    } else if let Some(rest) = path.as_str().strip_prefix(r"\\?\") {
        return Utf8PathBuf::from(rest);
    }
    path
}

pub(crate) async fn count_rows(pool: &SqlitePool, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let row: (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// 原本放在已删除的 `assets/tests/audio-fixtures` 目录里的音频夹具，改为在内存中
/// 合成。
///
/// 每个容器都从一个手工构造、符合规范的最小"外壳"开始（只有音频帧结构/元数据
/// 块），再用 lofty 自己的标签写入器挂上元数据，因此合成出的字节会走生产所用的
/// 那条读取路径往返。不附带任何二进制夹具文件。
pub(crate) mod audio_fixtures {
    use std::io::Cursor;

    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::id3::v2::Id3v2Tag;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::ItemKey;
    use lofty::tag::{Tag, TagExt, TagType};

    /// 返回具名夹具的合成字节。
    ///
    /// `cover.jpg` 是文件夹封面图。每个 `fixture.*` 曲目都带测试要断言的元数据：
    /// 完整那套（title/artist/album artist/album/genre/track 2 of 9/disc 1 of
    /// 3/ISRC/MusicBrainz album id/ReplayGain），并且每个容器都嵌入封面；日期在
    /// `flac/ogg/m4a/wav/aiff/opus`，歌词在 `flac/ogg/m4a/opus`（ID3 的 USLT 帧
    /// 不会被 lofty 归一化成 `Lyrics`，所以 MP3/AAC/WAV/AIFF 从来不暴露歌词，
    /// 与被删除的旧夹具一致）。
    pub(crate) fn fixture(name: &str) -> Vec<u8> {
        let (shell, tag_type, id3v23) = match name {
            "fixture.mp3" => (mp3_shell(), TagType::Id3v2, true),
            "fixture.aac" => (aac_shell(), TagType::Id3v2, true),
            "fixture.wav" => (wav_shell(), TagType::Id3v2, false),
            "fixture.aiff" => (aiff_shell(), TagType::Id3v2, false),
            "fixture.flac" => (flac_shell(), TagType::VorbisComments, false),
            "fixture.ogg" => (ogg_vorbis_shell(), TagType::VorbisComments, false),
            "fixture.opus" => (opus_shell(), TagType::VorbisComments, false),
            "fixture.m4a" => (mp4_shell(), TagType::Mp4Ilst, false),
            "cover.jpg" => return cover_jpeg(),
            other => panic!("unknown audio fixture: {other}"),
        };

        let with_date = matches!(
            name,
            "fixture.flac"
                | "fixture.ogg"
                | "fixture.m4a"
                | "fixture.wav"
                | "fixture.aiff"
                | "fixture.opus"
        );
        let with_lyrics = matches!(
            name,
            "fixture.flac" | "fixture.ogg" | "fixture.m4a" | "fixture.opus"
        );

        attach_tag(shell, tag_type, with_date, with_lyrics, id3v23)
    }

    /// 16x16 封面图，编码为 JPEG，供标签与文件夹封面文件使用。
    pub(crate) fn cover_jpeg() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(16, 16, image::Rgb([20, 120, 200]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        encoded.into_inner()
    }

    fn attach_tag(
        shell: Vec<u8>,
        tag_type: TagType,
        with_date: bool,
        with_lyrics: bool,
        id3v23: bool,
    ) -> Vec<u8> {
        let mut tag = Tag::new(tag_type);
        tag.insert_text(ItemKey::TrackTitle, "Test Track".to_string());
        tag.insert_text(ItemKey::TrackArtist, "Test Artist".to_string());
        tag.insert_text(ItemKey::AlbumArtist, "Test Album Artist".to_string());
        tag.insert_text(ItemKey::AlbumTitle, "Test Album".to_string());
        tag.insert_text(ItemKey::Genre, "Test Genre".to_string());
        tag.insert_text(ItemKey::TrackNumber, "2".to_string());
        tag.insert_text(ItemKey::TrackTotal, "9".to_string());
        tag.insert_text(ItemKey::DiscNumber, "1".to_string());
        tag.insert_text(ItemKey::DiscTotal, "3".to_string());
        tag.insert_text(ItemKey::Isrc, "QZHB12400001".to_string());
        tag.insert_text(
            ItemKey::MusicBrainzReleaseId,
            "12345678-1234-4234-9234-123456789abc".to_string(),
        );
        tag.insert_text(ItemKey::ReplayGainTrackGain, "-3.21 dB".to_string());
        tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.987654".to_string());
        tag.insert_text(ItemKey::ReplayGainAlbumGain, "-4.56 dB".to_string());
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, "0.876543".to_string());
        if with_date {
            tag.insert_text(ItemKey::RecordingDate, "1995-06-24".to_string());
        }
        if with_lyrics {
            tag.insert_text(ItemKey::Lyrics, "[00:00.00] Test lyrics".to_string());
        }
        tag.push_picture(
            Picture::unchecked(cover_jpeg())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );

        let mut file = Cursor::new(shell);
        let options = if id3v23 {
            WriteOptions::new().use_id3v23(true)
        } else {
            WriteOptions::new()
        };
        if tag_type == TagType::Id3v2 {
            // lofty 0.25.1 的统一 Tag -> ID3v2 转换会静默丢弃
            // `ItemKey::MusicBrainzReleaseId`（它的 TXXX 分支没列这个键，而通用
            // 回退只处理 4 字符帧 id），所以 MusicBrainz 的 TXXX 帧要手工加。
            // 读取端经 "MusicBrainz Album Id" 条目把它映射回来。
            let mut id3 = Id3v2Tag::from(tag);
            id3.insert_user_text(
                "MusicBrainz Album Id".to_string(),
                "12345678-1234-4234-9234-123456789abc".to_string(),
            );
            id3.save_to(&mut file, options)
                .expect("synthesized id3v2 tag must be writable");
        } else {
            let mut tagged = lofty::probe::Probe::new(&mut file)
                .guess_file_type()
                .expect("synthesized container shell must be guessable")
                .read()
                .expect("synthesized container shell must be readable");
            tagged.insert_tag(tag);
            tagged
                .save_to(&mut file, options)
                .expect("synthesized tag must be writable");
        }
        file.into_inner()
    }

    /// 四个 MPEG-1 Layer III 帧（44.1 kHz、128 kbps、单声道、零负载）。
    fn mp3_shell() -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..4 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
            // 帧长：144 * 128000 / 44100 = 417 字节（无填充）
            out.resize(out.len() + 413, 0);
        }
        out
    }

    /// ADTS 流：MPEG-4、AAC-LC、44.1 kHz（索引 4）、立体声、无 CRC。
    fn aac_shell() -> Vec<u8> {
        const PAYLOAD: usize = 100;
        let frame_len: u16 = 7 + PAYLOAD as u16;

        let mut out = Vec::new();
        for _ in 0..3 {
            out.extend_from_slice(&[0xFF, 0xF1]);
            out.push(0x48); // profile 01 (LC) | freq idx 0100 | private 0 | ch[2]
            out.push(0x80 | ((frame_len >> 11) as u8)); // ch[1:0] + flags + len[12:11]
            out.push((frame_len >> 3) as u8); // len[10:3]
            out.push((((frame_len & 0x7) as u8) << 5) | 0x1F); // len[2:0] + buffer fullness
            out.push(0xFC); // buffer fullness + one frame per ADTS packet
            out.resize(out.len() + PAYLOAD, 0);
        }
        out
    }

    /// 最小 RIFF/WAVE：一个 PCM fmt 块加一个小的静音 data 块。
    fn wav_shell() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // PCM
        body.extend_from_slice(&2u16.to_le_bytes()); // channels
        body.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
        body.extend_from_slice(&176_400u32.to_le_bytes()); // byte rate
        body.extend_from_slice(&4u16.to_le_bytes()); // block align
        body.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        body.extend_from_slice(b"data");
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 8]);

        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    /// 最小 FORM/AIFF：一个 COMM 块加一个小的静音 SSND 块。
    fn aiff_shell() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"AIFF");
        body.extend_from_slice(b"COMM");
        body.extend_from_slice(&18u32.to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes()); // channels
        body.extend_from_slice(&44_100u32.to_be_bytes()); // sample frames (1 s)
        body.extend_from_slice(&16u16.to_be_bytes()); // sample size
        body.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]); // 44100 Hz, 80-bit float
        body.extend_from_slice(b"SSND");
        body.extend_from_slice(&16u32.to_be_bytes()); // offset + block size + data
        body.extend_from_slice(&0u32.to_be_bytes()); // offset
        body.extend_from_slice(&0u32.to_be_bytes()); // block size
        body.extend_from_slice(&[0u8; 8]); // silence

        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend(body);
        out
    }

    /// 只有元数据的 FLAC：`fLaC` 标记 + STREAMINFO（1 秒的 16-bit 立体声
    /// 44.1 kHz，好让元数据里的时长可用）+ 一个空的 PADDING 块。
    fn flac_shell() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        // STREAMINFO（类型 0），内容 34 字节，不是最后一块
        out.extend_from_slice(&[0, 0, 0, 34]);
        let mut streaminfo = [0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes()); // min blocksize
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max blocksize
        // min/max 帧长保持 0（未知）；MD5 保持 0（未知）
        let packed = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100u64; // rate/ch/bps/total samples
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&streaminfo);
        // PADDING（类型 1），空，最后一块
        out.extend_from_slice(&[0x81, 0, 0, 0]);
        // 一个裸帧头，没有 subframe。symphonia 在打开时要把 packet 解析器同步到
        // 第一个音频帧，所以只有元数据的流会撞到 EOF 并让 probe 失败。这里用
        // fixed-blocksize 策略：帧号 0、块长码 0xC（即 STREAMINFO 钉住的 4096）、
        // 44.1 kHz / 2 ch / 16 bits、CRC-8 覆盖前面的字节。
        let header = [0xFF, 0xF8, 0xC9, 0x18, 0x00];
        out.extend_from_slice(&header);
        out.push(crc8(&header));
        out
    }

    /// CRC-8，用 ATM/CCITT 多项式（0x07，初值 0），FLAC 帧头正是用它校验的。
    fn crc8(data: &[u8]) -> u8 {
        let mut crc = 0u8;
        for &byte in data {
            crc ^= byte;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 {
                    (crc << 1) ^ 0x07
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// Ogg Vorbis，带三个强制的头部包（identification、comment、setup 占位）。
    /// lofty 会校验前两个签名。
    fn ogg_vorbis_shell() -> Vec<u8> {
        let mut ident = Vec::new();
        ident.extend_from_slice(b"\x01vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes()); // vorbis version
        ident.push(2); // channels
        ident.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate
        ident.extend_from_slice(&0i32.to_le_bytes()); // bitrate max
        ident.extend_from_slice(&96_000i32.to_le_bytes()); // bitrate nominal
        ident.extend_from_slice(&0i32.to_le_bytes()); // bitrate min
        ident.push(0xB8); // blocksize 2048/256
        ident.push(1); // framing bit

        let mut comment = Vec::new();
        comment.extend_from_slice(b"\x03vorbis");
        comment.extend_from_slice(&7u32.to_le_bytes());
        comment.extend_from_slice(b"meliora");
        comment.extend_from_slice(&0u32.to_le_bytes()); // comment count
        comment.push(1); // framing bit

        let mut setup = Vec::new();
        setup.extend_from_slice(b"\x05vorbis");
        setup.extend_from_slice(&[0; 4]);

        let mut out = Vec::new();
        ogg_page(&mut out, &ident, true, 0);
        ogg_page(&mut out, &comment, false, 1);
        ogg_page(&mut out, &setup, false, 2);
        out
    }

    /// Ogg Opus，带两个强制的头部包。
    fn opus_shell() -> Vec<u8> {
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(2); // channels
        head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family

        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&7u32.to_le_bytes());
        tags.extend_from_slice(b"meliora");
        tags.extend_from_slice(&0u32.to_le_bytes()); // comment count

        let mut out = Vec::new();
        ogg_page(&mut out, &head, true, 0);
        ogg_page(&mut out, &tags, false, 1);
        out
    }

    /// 单包 Ogg 页，CRC 正确。
    fn ogg_page(out: &mut Vec<u8>, packet: &[u8], bos: bool, sequence: u32) {
        assert!(
            packet.len() < 255,
            "this builder emits single-segment pages"
        );

        let mut page = Vec::with_capacity(28 + packet.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // stream structure version
        page.push(if bos { 0x02 } else { 0 }); // header type flags
        page.extend_from_slice(&0u64.to_le_bytes()); // granule position
        page.extend_from_slice(&0x1D_C0_DEu32.to_le_bytes()); // bitstream serial
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        page.push(1); // segment count
        page.push(packet.len() as u8); // segment table
        page.extend_from_slice(packet);

        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&page);
    }

    /// Ogg 的 CRC：多项式 0x04c11db7，MSB 优先，末尾不取反。
    fn ogg_crc(data: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &byte in data {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// 最小 MP4：`ftyp` 加一个含音频轨骨架的 `moov`
    /// （`trak > mdia > hdlr(soun) + mdhd`）。lofty 的标签写入器在保存时会补上
    /// `udta > meta > ilst` 这棵树。
    fn mp4_shell() -> Vec<u8> {
        fn atom(ident: &[u8; 4], content: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + content.len());
            out.extend_from_slice(&((8 + content.len()) as u32).to_be_bytes());
            out.extend_from_slice(ident);
            out.extend_from_slice(content);
            out
        }

        let mut ftyp_content = Vec::new();
        ftyp_content.extend_from_slice(b"isom"); // major brand
        ftyp_content.extend_from_slice(&0x200u32.to_be_bytes()); // minor version
        ftyp_content.extend_from_slice(b"isom"); // compatible brand
        let ftyp = atom(b"ftyp", &ftyp_content);

        let mut hdlr_content = Vec::new();
        hdlr_content.extend_from_slice(&[0; 8]); // version/flags + pre-defined
        hdlr_content.extend_from_slice(b"soun"); // handler type
        let hdlr = atom(b"hdlr", &hdlr_content);

        let mut mdhd_content = Vec::new();
        mdhd_content.extend_from_slice(&[0; 4]); // version 0 + flags
        mdhd_content.extend_from_slice(&[0; 8]); // creation + modification time
        mdhd_content.extend_from_slice(&44_100u32.to_be_bytes()); // timescale
        mdhd_content.extend_from_slice(&44_100u32.to_be_bytes()); // duration (1 s)
        mdhd_content.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        mdhd_content.extend_from_slice(&[0; 2]); // pre-defined
        let mdhd = atom(b"mdhd", &mdhd_content);

        let mut mdia_content = hdlr;
        mdia_content.extend_from_slice(&mdhd);
        let mdia = atom(b"mdia", &mdia_content);
        let trak = atom(b"trak", &mdia);
        let moov = atom(b"moov", &trak);

        let mut out = ftyp;
        out.extend_from_slice(&moov);
        out
    }
}

/// 手工运行的微基准，不隶属于某个单一模块。运行方式：
/// `cargo test --release --features kugou -- bench_image_cache --ignored --nocapture`
///
/// 实测数字只在真实跑过之后才写进各测试的文档注释
/// （GPUI HARDCORE §1：没有数据就没有结论——绝不预填）。
mod benches {
    use std::collections::VecDeque;
    use std::hint::black_box;
    use std::time::Instant;

    use rustc_hash::{FxBuildHasher, FxHashMap};

    /// 量化 `MelioraImageCache::load`（src/ui/caching.rs）LRU 队首快路径的收益：
    /// 命中已经在 `usage_list` 队首的条目可以跳过 O(depth) 的 `position` 扫描 +
    /// `remove` + `push_front` 重排；其他每次命中都要付。200 条对应封面表的
    /// `meliora_cache(..., 200)` 调用点（table.rs）。
    ///
    /// 下面的结构精确复刻命中分支。刻意排除的项（冷热路径完全一致，会互相抵消）：
    /// resource 的 `gpui::hash` 与 `ImageCacheItem::get()`。
    ///
    /// 测量有效性说明（这里用朴素做法会静默量到零）：
    /// - 命中键逐次操作都过 `black_box`，否则 LLVM 会把整个循环不变的热门命中
    ///   （map 查找 + 队首检查都不改动状态）提升出计时循环；
    /// - 中/冷批次重放一个"自维持循环"：反复命中当前正好位于 `depth` 的那个条目，
    ///   会把前 depth+1 个条目转回它们的起始状态，于是*每次*计时操作都是货真价实的
    ///   depth-`d` 命中。若反复命中同一个键，它首次操作后就到了队首，量到的反而是
    ///   快路径。
    struct LruMirror {
        usage_list: VecDeque<u64>,
        // 用 32 字节替身模拟 `(ImageCacheItem, Resource)` 的 map 负载
        cache: FxHashMap<u64, [u64; 4]>,
    }

    const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;

    impl LruMirror {
        fn new(items: usize) -> Self {
            let mut usage_list = VecDeque::with_capacity(items);
            let mut cache = FxHashMap::with_capacity_and_hasher(items, FxBuildHasher);
            for i in 0..items as u64 {
                let hash = i.wrapping_mul(SPREAD);
                usage_list.push_back(hash);
                cache.insert(hash, [i; 4]);
            }
            Self { usage_list, cache }
        }

        /// 返回该条目被找到时的深度（0 = 队首 / 热）。
        fn hit(&mut self, hash: u64) -> usize {
            if let Some(_item) = self.cache.get_mut(&hash) {
                if self.usage_list.front() != Some(&hash) {
                    let idx = self
                        .usage_list
                        .iter()
                        .position(|item| *item == hash)
                        .expect("cache has an item usage_list doesn't");
                    self.usage_list.remove(idx);
                    self.usage_list.push_front(hash);
                    return idx;
                }
                return 0;
            }
            panic!("hit on missing item");
        }

        /// 每条命中路径都要付的下界：只做 map 查找、不含 LRU 记账。
        fn lookup_only(&self, hash: u64) -> bool {
            self.cache.contains_key(&hash)
        }

        /// 预计算出一条操作序列，让每次命中都正好落在 `depth`，且序列跑完后 LRU
        /// 回到起始状态（逐次操作断言）。
        fn depth_cycle(&mut self, depth: usize) -> Vec<u64> {
            let initial: Vec<u64> = self.usage_list.iter().copied().collect();
            let mut cycle = Vec::new();
            loop {
                let target = self.usage_list[depth];
                assert_eq!(
                    self.hit(target),
                    depth,
                    "cycle construction hit at an unexpected depth"
                );
                cycle.push(target);
                if self.usage_list.iter().copied().eq(initial.iter().copied()) {
                    return cycle;
                }
            }
        }
    }

    const ITEMS: usize = 200;
    const OPS_PER_BATCH: usize = 200;
    const WARMUP_BATCHES: usize = 2;
    const BATCHES: usize = 20;

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }

    /// 实测于 2026-09-12（release，20 批 x 200 次操作取中位数，200 条）：裸 map
    /// 查找 2.0 ns，热/队首命中 3.5 ns，depth-100 命中 64.0 ns（+60.5 ns），
    /// depth-199 命中 90.0 ns（+86.5 ns）。队首检查让 LRU-热的命中与裸查找下界
    /// 只差约 1.5 ns，而链表尾部的命中要付热路径约 26 倍的成本——快路径为热条目
    /// 消除了约 96% 的命中成本，而真实命中绝大多数都落在热区（可见网格），所以
    /// caching.rs 里那层守卫是有据的。
    #[test]
    #[ignore = "benchmark: run with --ignored"]
    fn bench_image_cache_lru_fast_path() {
        // usage_list 深度 = 条目下标（队首 = 0）。深度档位对齐任务书：热（队首 /
        // 快路径）、缓存中部、链表尾部冷。
        let cases: [(&str, usize); 3] = [
            ("hot  (front, fast path)", 0),
            ("warm (depth 100)        ", 100),
            ("cold (depth 199)        ", 199),
        ];

        let mut lru = LruMirror::new(ITEMS);
        let mut results = Vec::new();

        // 仅查找的下界，用同一结构测（键对外不透明）
        let hash = 100u64.wrapping_mul(SPREAD);
        let mut floor_samples = Vec::new();
        for batch in 0..WARMUP_BATCHES + BATCHES {
            let start = Instant::now();
            for _ in 0..OPS_PER_BATCH {
                black_box(lru.lookup_only(black_box(hash)));
            }
            if batch >= WARMUP_BATCHES {
                floor_samples.push(start.elapsed().as_secs_f64() * 1e9 / OPS_PER_BATCH as f64);
            }
        }
        results.push(("map lookup only (floor)  ", median(floor_samples)));

        for (label, depth) in cases {
            let cycle = lru.depth_cycle(depth);
            let mut samples = Vec::new();
            let mut op = 0usize; // cumulative across batches: the cycle must
            // 与 LRU 的轮转状态保持一致
            for batch in 0..WARMUP_BATCHES + BATCHES {
                let start = Instant::now();
                for _ in 0..OPS_PER_BATCH {
                    black_box(lru.hit(black_box(cycle[op % cycle.len()])));
                    op += 1;
                }
                if batch >= WARMUP_BATCHES {
                    samples.push(start.elapsed().as_secs_f64() * 1e9 / OPS_PER_BATCH as f64);
                }
            }
            results.push((label, median(samples)));
        }

        let hot = results[1].1;
        println!(
            "MelioraImageCache LRU hit path, {ITEMS} entries, ns/op (median of {BATCHES} batches x {OPS_PER_BATCH} ops):"
        );
        for (label, ns) in &results {
            let delta = if label.starts_with("warm") || label.starts_with("cold") {
                format!("  ({:+.1} ns vs hot)", ns - hot)
            } else {
                String::new()
            };
            println!("  {label} {ns:8.1} ns{delta}");
        }
    }
}

/// 大库规模实测：设计目标（搜索索引"数秒"、可用性快照全表查询、单专辑
/// 取曲）全部按 10 万曲设计，但从未在本机验证过——真实库只有 53 张
/// 专辑。默认 #[ignore]（灌库约 1 分钟，不拖常规测试轮），显式运行：
/// `cargo test --release --features kugou scale_probe -- --ignored --nocapture`
/// 灌库走真实写入路径（update_metadata + artist flush，单事务）。
#[cfg(test)]
mod scale_probe {
    use super::*;
    use crate::ui::availability::compute_availability_from_tracks;
    use sqlx::Acquire;
    use std::time::Instant;

    const TRACKS: usize = 100_000;
    const ALBUMS: usize = 10_000;

    #[tokio::test]
    #[ignore = "scale probe: ~1-2 min, explicit run only"]
    async fn hot_read_paths_at_100k_tracks() {
        let (dir, pool) = create_test_pool("meliora-scale-probe").await;

        // ---- 灌库：单事务 + 真实写入路径 ----
        let fill_started = Instant::now();
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut tx = conn.begin().await.unwrap();
            for i in 0..TRACKS {
                let album_idx = i % ALBUMS;
                let path = dir.utf8_join(&format!("t{i:05}.flac"));
                insert_metadata(
                    &mut tx,
                    &track_metadata(
                        &format!("Album {album_idx:05}"),
                        &format!("Artist {}", album_idx % 1_000),
                        &format!("Track {i:05}"),
                        (i % 20) as u64 + 1,
                    ),
                    &path,
                )
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
        }
        let fill = fill_started.elapsed();
        let (tracks_n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM track")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (albums_n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album")
            .fetch_one(&pool)
            .await
            .unwrap();
        println!(
            "灌库: {tracks_n} 曲 / {albums_n} 专辑，耗时 {:.2}s",
            fill.as_secs_f64()
        );

        // ---- 1) 可用性快照基查询 ----
        let t = Instant::now();
        let track_rows = db::list_all_tracks(&pool).await.unwrap();
        println!(
            "1) list_all_tracks（{} 行）: {:?}",
            track_rows.len(),
            t.elapsed()
        );

        // ---- 2) 可用性 stat pass（10 万去重路径 exists）----
        let credits = db::list_track_artist_credits(&pool).await.unwrap();
        let credits_n = credits.len();
        let t = Instant::now();
        let (albums, artists, tracks) =
            compute_availability_from_tracks(track_rows.clone(), credits);
        println!(
            "2) 可用性 stat pass（{credits_n} credits）: {:?} → albums {} / artists {} / tracks {}",
            t.elapsed(),
            albums.len(),
            artists.len(),
            tracks.len()
        );

        // ---- 3) 搜索索引四查询 ----
        let t = Instant::now();
        let avail = db::list_album_availability(&pool).await.unwrap();
        let albums_q = db::list_albums_search(&pool).await.unwrap();
        let artists_q = db::list_artists_search(&pool).await.unwrap();
        let tracks_q = db::list_tracks_search(&pool).await.unwrap();
        println!(
            "3) 搜索索引四查询: {:?}（avail {} / albums {} / artists {} / tracks {}）",
            t.elapsed(),
            avail.len(),
            albums_q.len(),
            artists_q.len(),
            tracks_q.len()
        );

        // ---- 4) 单专辑取曲（详情页热路径）----
        let t = Instant::now();
        let (album_id,): (i64,) = sqlx::query_as("SELECT id FROM album LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let in_album = db::list_tracks_in_album(&pool, album_id).await.unwrap();
        println!(
            "4) list_tracks_in_album（{} 行）: {:?}",
            in_album.len(),
            t.elapsed()
        );

        // 结构健全性：灌库比率符合设计（10 曲/专辑，专辑/艺术家去重生效）。
        assert_eq!(tracks_n as usize, TRACKS);
        assert_eq!(albums_n as usize, ALBUMS);
    }

    /// 排序路径 100k 实测 + 候选索引对照实验。先量现状（第十二轮的 SQL 审计显示
    /// 这批查询的计划是 `SCAN` + `USE TEMP B-TREE FOR ORDER BY`），再在**同一个库**
    /// 里建候选索引复量，并打印建索引本身的耗时（它就是将来迁移的成本）。
    /// 索引要不要进 migrations，只由这张前后对照表决定（§1/§37/§38）。
    /// 执行计划由 python 侧对同一批 SQL 跑 EXPLAIN 复核，不在此处重复。
    #[tokio::test]
    #[ignore = "scale probe: ~10 min fill + sort A/B, explicit run only"]
    async fn sort_paths_at_100k_with_index_experiment() {
        const SORTS: &[(&str, &str)] = &[
            (
                "track title asc",
                include_str!("../queries/library/find_tracks_title_asc.sql"),
            ),
            (
                "track title desc",
                include_str!("../queries/library/find_tracks_title_desc.sql"),
            ),
            (
                "track length asc",
                include_str!("../queries/library/find_tracks_length_asc.sql"),
            ),
            (
                "track number asc",
                include_str!("../queries/library/find_tracks_number_asc.sql"),
            ),
            (
                "track album asc",
                include_str!("../queries/library/find_tracks_album_asc.sql"),
            ),
            (
                "track artist asc (已判定接受)",
                include_str!("../queries/library/find_tracks_artist_asc.sql"),
            ),
            (
                "album title asc (已是有序索引扫描)",
                include_str!("../queries/library/find_albums_title_asc.sql"),
            ),
            (
                "album release asc",
                include_str!("../queries/library/find_albums_release_asc.sql"),
            ),
            (
                "album label asc",
                include_str!("../queries/library/find_albums_label_asc.sql"),
            ),
            (
                "album catnum asc",
                include_str!("../queries/library/find_albums_catnum_asc.sql"),
            ),
            (
                "album artist asc",
                include_str!("../queries/library/find_albums_artist_asc.sql"),
            ),
            (
                "artist name asc",
                include_str!("../queries/library/find_artists_name_asc.sql"),
            ),
        ];

        // 候选索引：与第十二轮 SQL 审计里"排序键全在本表内"的 6 组一一对应。
        const CANDIDATES: &[(&str, &str)] = &[
            (
                "bench_track_title",
                "CREATE INDEX bench_track_title ON track(title_sortable, album_id, location COLLATE NOCASE)",
            ),
            (
                "bench_track_duration",
                "CREATE INDEX bench_track_duration ON track(duration)",
            ),
            (
                "bench_track_discnum",
                "CREATE INDEX bench_track_discnum ON track(disc_number, track_number)",
            ),
            (
                "bench_album_release_title",
                "CREATE INDEX bench_album_release_title ON album(release_date, title_sortable COLLATE NOCASE)",
            ),
            (
                "bench_album_label",
                "CREATE INDEX bench_album_label ON album(label COLLATE NOCASE, catalog_number, release_date)",
            ),
            (
                "bench_album_catnum",
                "CREATE INDEX bench_album_catnum ON album(catalog_number COLLATE NOCASE, release_date)",
            ),
        ];

        async fn time_sorts(pool: &SqlitePool, phase: &str) -> Vec<(String, u128, usize)> {
            let mut out = Vec::new();
            for &(label, sql) in SORTS {
                let mut best = u128::MAX;
                let mut rows = 0usize;
                for _ in 0..3 {
                    let t = Instant::now();
                    let fetched = sqlx::query(sql).fetch_all(pool).await.unwrap();
                    best = best.min(t.elapsed().as_millis());
                    rows = fetched.len();
                }
                println!("  [{phase}] {label:34} min={best:5} ms  rows={rows}");
                out.push((label.to_string(), best, rows));
            }
            out
        }

        let (dir, pool) = create_test_pool("meliora-sort-probe").await;

        let fill_started = Instant::now();
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut tx = conn.begin().await.unwrap();
            for i in 0..TRACKS {
                let album_idx = i % ALBUMS;
                let path = dir.utf8_join(&format!("t{i:05}.flac"));
                insert_metadata(
                    &mut tx,
                    &track_metadata(
                        &format!("Album {album_idx:05}"),
                        &format!("Artist {}", album_idx % 1_000),
                        &format!("Track {i:05}"),
                        (i % 20) as u64 + 1,
                    ),
                    &path,
                )
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
        }
        println!(
            "灌库: {TRACKS} 曲 / {ALBUMS} 专辑，耗时 {:.1}s",
            fill_started.elapsed().as_secs_f64()
        );

        println!("== 现状（无候选索引）==");
        let before = time_sorts(&pool, "before").await;

        println!("== 建候选索引的耗时（= 将来迁移成本）==");
        for &(name, ddl) in CANDIDATES {
            let t = Instant::now();
            sqlx::query(ddl).execute(&pool).await.unwrap();
            println!("  {name:26} {:6}.0 ms", t.elapsed().as_millis().max(1));
        }

        println!("== 建索引后 ==");
        let after = time_sorts(&pool, "after").await;

        println!("== 对照（正数=建索引后省下的 ms）==");
        for ((label, b, _), (_, a, _)) in before.iter().zip(after.iter()) {
            println!("  {label:34} {b:5} → {a:5} ms  Δ={}", *b as i64 - *a as i64);
        }
    }

    /// 播放歌单点击的主线程成本曲线。`find_playlist_tracks`
    /// （`ui/library/playlist_view.rs`）在一次点击里做两件事，都在主线程：
    /// ① `list_playlist_tracks` 的 `block_on` 取行（JOIN + ORDER BY position），
    /// ② 逐行 `QueueItemData::new`（每行一次 String→PathBuf 加一次
    /// `Arc<AtomicI64>` 分配）。日志侧的 `[db] slow ui-thread query`（阈值 2ms）
    /// 只证过"本机小库从未越线"，这条给出规模曲线：歌单/库长大时主线程
    /// 会被这次点击堵住多久。默认 #[ignore]：灌 10k 曲并建歌单要 1-2 分钟。
    /// `cargo test --release --features kugou report_playlist_click_costs -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "cost probe: playlist play-click on the main thread, explicit run only"]
    async fn report_playlist_click_costs_at_scale() {
        use crate::playback::queue::QueueItemData;

        let (dir, pool) = create_test_pool("meliora-playlist-click").await;
        for size in [500usize, 2000, 5000, 10000] {
            {
                let mut conn = pool.acquire().await.unwrap();
                let mut tx = conn.begin().await.unwrap();
                for i in 0..size {
                    let path = dir.utf8_join(&format!("p{i:05}.flac"));
                    insert_metadata(
                        &mut tx,
                        &track_metadata(
                            "Click Album",
                            "Click Artist",
                            &format!("Click {i:05}"),
                            (i % 20) as u64 + 1,
                        ),
                        &path,
                    )
                    .await
                    .unwrap();
                }
                tx.commit().await.unwrap();
            }

            // 歌单名有唯一约束，每档各建一个
            let playlist_id = db::create_playlist(&pool, &format!("click-probe-{size}"))
                .await
                .unwrap();
            let seed_started = Instant::now();
            {
                // 批量单事务 + 显式 checkpoint：旧写法每行一个事务，测出来的
                // "查询耗时"里混进了未 checkpoint 的 WAL 索引构建（同一探针曾把
                // 5k 行报成 20ms、10k 行报成 102ms，而干净口径的真值是 6ms/12ms）。
                let track_ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM track ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                let mut conn = pool.acquire().await.unwrap();
                let mut tx = conn.begin().await.unwrap();
                for (rank, (track_id,)) in track_ids.iter().enumerate() {
                    sqlx::query(
                        "INSERT INTO playlist_item (playlist_id, track_id, position) \
                         VALUES ($1, $2, $3)",
                    )
                    .bind(playlist_id)
                    .bind(*track_id)
                    .bind(rank as i64)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                }
                tx.commit().await.unwrap();
                sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            let seeding = seed_started.elapsed();

            // min-of-3：本机 run-to-run ±20%（见记忆 bench-noise-and-paired-ab）。
            let mut query = std::time::Duration::MAX;
            let mut build = std::time::Duration::MAX;
            let mut item_count = 0usize;
            for _ in 0..3 {
                let started = Instant::now();
                let rows = db::list_playlist_tracks(&pool, playlist_id).await.unwrap();
                query = query.min(started.elapsed());

                let started = Instant::now();
                let items: Vec<QueueItemData> = rows
                    .iter()
                    .map(|row| {
                        QueueItemData::new(
                            row.location.clone().into(),
                            Some(row.track_id),
                            Some(row.album_id),
                        )
                    })
                    .collect();
                build = build.min(started.elapsed());
                item_count = items.len();
            }

            println!(
                "playlist_click size={size} query_ms={:.2} build_ms={:.2} click_total_ms={:.2} us_per_row={:.2} items={} (seeding {:.1}s)",
                query.as_secs_f64() * 1000.0,
                build.as_secs_f64() * 1000.0,
                (query.as_secs_f64() + build.as_secs_f64()) * 1000.0,
                (query.as_micros() + build.as_micros()) as f64 / size as f64,
                item_count,
                seeding.as_secs_f64(),
            );

            // 分离变量看超线性来自哪里：①去掉 ORDER BY（有序输出 => 随机回查
            // track 行 的形态消失）；②只取索引内的列（覆盖索引，零行回查）。
            // 'static 常量字符串让 EXPLAIN 与计时共用同一条 SQL，不引借用期问题。
            const NO_SORT: &str = "SELECT pl.id, pl.track_id, t.album_id, t.location \
                 FROM playlist_item as pl JOIN track t on pl.track_id = t.id \
                 WHERE pl.playlist_id = $1";
            const COVERING: &str = "SELECT pl.id, pl.track_id, pl.position FROM playlist_item pl \
                 WHERE pl.playlist_id = $1 ORDER BY pl.position ASC";

            if size >= 5000 {
                let started = Instant::now();
                let rows4: Vec<(i64, i64, i64, String)> = sqlx::query_as(NO_SORT)
                    .bind(playlist_id)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                println!(
                    "  variant no_order_by      size={size} ms={:.2} rows={}",
                    started.elapsed().as_secs_f64() * 1000.0,
                    rows4.len()
                );
                let started = Instant::now();
                let rows3: Vec<(i64, i64, i64)> = sqlx::query_as(COVERING)
                    .bind(playlist_id)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                println!(
                    "  variant covering_no_join size={size} ms={:.2} rows={}",
                    started.elapsed().as_secs_f64() * 1000.0,
                    rows3.len()
                );

                const EXPLAIN_SHIPPED: &str = concat!(
                    "EXPLAIN QUERY PLAN ",
                    include_str!("../queries/playlist/get_track_listing.sql")
                );
                const EXPLAIN_NO_SORT: &str = concat!(
                    "EXPLAIN QUERY PLAN SELECT pl.id, pl.track_id, t.album_id, t.location ",
                    "FROM playlist_item as pl JOIN track t on pl.track_id = t.id ",
                    "WHERE pl.playlist_id = $1"
                );
                const EXPLAIN_COVERING: &str = concat!(
                    "EXPLAIN QUERY PLAN SELECT pl.id, pl.track_id, pl.position ",
                    "FROM playlist_item pl WHERE pl.playlist_id = $1 ORDER BY pl.position ASC"
                );
                for (label, sql) in [
                    ("as_shipped", EXPLAIN_SHIPPED),
                    ("no_order_by", EXPLAIN_NO_SORT),
                    ("covering_no_join", EXPLAIN_COVERING),
                ] {
                    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sql)
                        .bind(playlist_id)
                        .fetch_all(&pool)
                        .await
                        .unwrap();
                    for row in plan {
                        println!("  plan[{label}] {}", row.3);
                    }
                }
            }
        }
    }

    /// 会话快照的真实单价：`send_session_with_queue` 每次**深拷贝整条队列**
    /// 再序列化落盘（`queue_manager/session.rs` 顶部那条注释承认这点，并用
    /// 500ms 前缘防抖把突发压成"每窗口一次快照"）。本机歌单只有 20 项，
    /// 所以"随机播放全部 10 万曲"那档从来没人量过：一次快照 = 一次
    /// O(队列) 深拷贝（发生在喂音频环的那条线程上）+ 一次几十 MB 序列化。
    /// 这条给出 clone / serialize / bytes 三个数，用来判那个极端场景是否
    /// 会吃掉 250ms 的环余量。默认 #[ignore]（100k 档要分配几十 MB）。
    /// `cargo test --release --features kugou report_session_snapshot -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "cost probe: session snapshot clone + serialize at queue scale, explicit run only"]
    async fn report_session_snapshot_costs() {
        use crate::playback::queue::QueueItemData;

        for n in [20usize, 1_000, 25_000, 100_000] {
            let queue: Vec<QueueItemData> = (0..n)
                .map(|i| {
                    QueueItemData::new(
                        format!("D:/probe/artist {}/album y/track_{i:05}.flac", i % 400).into(),
                        Some(i as i64),
                        Some((i % 50) as i64),
                    )
                })
                .collect();

            let started = Instant::now();
            let cloned = queue.clone();
            let clone = started.elapsed();

            let started = Instant::now();
            let json = serde_json::to_vec(&cloned).unwrap_or_default();
            let serialize = started.elapsed();

            println!(
                "session_snapshot n={n} clone_ms={:.2} serialize_ms={:.2} bytes={} B_per_item={:.0} note=per-window-worst-2-clones(queue+original)",
                clone.as_secs_f64() * 1000.0,
                serialize.as_secs_f64() * 1000.0,
                json.len(),
                json.len() as f64 / n as f64,
            );
        }
    }
}
