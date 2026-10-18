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

    /// Run `f` with allocation counting suspended.
    ///
    /// This is needed to prevent allocations from other libraries from being
    /// counted as part of Meliora's own decode/convert path.
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
            // don't care about these right now
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
        // unique per test process: a leftover dir from a crashed/killed run (Windows can refuse
        // removal while SQLite handles close) must not be reused with its stale contents
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

/// Registers the built-in media providers exactly once per test process.
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

/// Seeds a synthetic library with raw SQL in one transaction: `artists`
/// artists, `albums` albums (each linked to its same-numbered artist) and
/// `albums * tracks_per_album` tracks (each linked to its album's artist).
/// The shape matches a real scan write closely enough for read-path
/// benchmarks (10k tracks ≈ 200 albums × 50); the write path itself is
/// covered by the scan tests and the 100k `scale_probe`.
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

/// Untimed warmup runs before timing; keeps first-call costs (page cache
/// population, lazy statics) out of the samples.
pub(crate) const BENCH_WARMUP: usize = 50;
/// Timed runs per probe; headline numbers use the min.
pub(crate) const BENCH_ITERATIONS: usize = 400;

/// Times `f` after `BENCH_WARMUP` untimed runs and prints a
/// min/p50/p90/p99/max/mean summary in microseconds — the micro-query
/// format the per-row benches read in.
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

/// [`bench_summarize`] in milliseconds with explicit run counts — for
/// probes whose single run costs 10s of ms (image pipelines, index
/// builds), where the default 400 iterations would take minutes.
/// Percentiles collapse to min/mean below 10 runs.
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

/// Strip the Windows verbatim (`\\?\`) prefix that `std::fs::canonicalize`
/// produces, so the result compares equal to the plain spelling. A no-op on
/// other platforms and for paths that carry no prefix.
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

/// In-memory synthesis of the audio fixtures that used to live in the deleted
/// `assets/tests/audio-fixtures` directory.
///
/// Every container starts as a hand-built, spec-conformant minimal "shell"
/// (audio framing / metadata blocks only) and gets its metadata attached with
/// lofty's own tag writers, so the synthesized bytes round-trip through
/// exactly the read path production uses. No binary fixture files are shipped.
pub(crate) mod audio_fixtures {
    use std::io::Cursor;

    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::id3::v2::Id3v2Tag;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::ItemKey;
    use lofty::tag::{Tag, TagExt, TagType};

    /// Returns the synthesized bytes of the named fixture.
    ///
    /// `cover.jpg` is the folder-art image. Each `fixture.*` track carries the
    /// metadata the tests assert on: the rich set (title/artist/album artist/
    /// album/genre/track 2 of 9/disc 1 of 3/ISRC/MusicBrainz album id/
    /// ReplayGain) and an embedded cover on every container, plus the date on
    /// `flac/ogg/m4a/wav/aiff/opus` and lyrics on `flac/ogg/m4a/opus` (ID3's
    /// USLT frame is not normalized to `Lyrics` by lofty, so MP3/AAC/WAV/AIFF
    /// never exposed lyrics, matching the deleted fixtures).
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

    /// 16x16 cover image encoded as JPEG, for tags and folder-art files.
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
            // lofty 0.25.1's unified Tag -> ID3v2 conversion silently drops
            // `ItemKey::MusicBrainzReleaseId` (its TXXX branch does not list
            // that key and the generic fallback only handles 4-char frame
            // ids), so the MusicBrainz TXXX frame is added by hand. The
            // reader maps it back through the "MusicBrainz Album Id" entry.
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

    /// Four MPEG-1 Layer III frames (44.1 kHz, 128 kbps, mono, zero payload).
    fn mp3_shell() -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..4 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
            // frame length: 144 * 128000 / 44100 = 417 bytes (no padding)
            out.resize(out.len() + 413, 0);
        }
        out
    }

    /// ADTS stream: MPEG-4, AAC-LC, 44.1 kHz (index 4), stereo, no CRC.
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

    /// Minimal RIFF/WAVE: PCM fmt chunk plus a small silent data chunk.
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

    /// Minimal FORM/AIFF: COMM chunk plus a small silent SSND chunk.
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

    /// Metadata-only FLAC: `fLaC` marker + STREAMINFO (1 s of 16-bit stereo
    /// 44.1 kHz, so the metadata duration is usable) + an empty PADDING block.
    fn flac_shell() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34-byte content, not the last block
        out.extend_from_slice(&[0, 0, 0, 34]);
        let mut streaminfo = [0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes()); // min blocksize
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max blocksize
        // min/max frame size stay 0 (unknown); MD5 stays 0 (unknown)
        let packed = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100u64; // rate/ch/bps/total samples
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&streaminfo);
        // PADDING (type 1), empty, last block
        out.extend_from_slice(&[0x81, 0, 0, 0]);
        // A bare frame header, no subframes. symphonia synchronizes its packet
        // parser to the first audio frame while opening, so a metadata-only
        // stream runs into EOF and fails the probe. Fixed-blocksize strategy
        // with frame number 0, block size code 0xC (= the 4096 the STREAMINFO
        // pins), 44.1 kHz / 2 ch / 16 bits, CRC-8 over the preceding bytes.
        let header = [0xFF, 0xF8, 0xC9, 0x18, 0x00];
        out.extend_from_slice(&header);
        out.push(crc8(&header));
        out
    }

    /// CRC-8 with the ATM/CCITT polynomial (0x07, init 0) that FLAC frame
    /// headers are checksummed with.
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

    /// Ogg Vorbis with the three mandatory header packets (identification,
    /// comment, setup placeholder). lofty verifies the first two signatures.
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

    /// Ogg Opus with the two mandatory header packets.
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

    /// A single-packet Ogg page with a correct CRC.
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

    /// The Ogg CRC: poly 0x04c11db7, MSB-first, no final inversion.
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

    /// Minimal MP4: `ftyp` plus a `moov` containing an audio track skeleton
    /// (`trak > mdia > hdlr(soun) + mdhd`). lofty's tag writer appends the
    /// `udta > meta > ilst` tree on save.
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

/// Hand-run micro-benchmarks that are not tied to a single module. Run with:
/// `cargo test --release --features kugou -- bench_image_cache --ignored --nocapture`
///
/// Measured numbers are recorded in each test's doc comment after actual runs
/// only (GPUI HARDCORE §1: no numbers, no conclusion - never pre-fill).
mod benches {
    use std::collections::VecDeque;
    use std::hint::black_box;
    use std::time::Instant;

    use rustc_hash::{FxBuildHasher, FxHashMap};

    /// Quantifies the LRU-front fast path of `MelioraImageCache::load`
    /// (src/ui/caching.rs): a hit for the item already at the front of
    /// `usage_list` skips the O(depth) `position` scan + `remove` +
    /// `push_front` reorder; every other hit pays it. 200 entries matches the
    /// artwork tables' `meliora_cache(..., 200)` call site (table.rs).
    ///
    /// The structure below mirrors the hit branch exactly. Excluded on purpose
    /// (identical across hot/cold paths, so they cancel out): `gpui::hash`
    /// of the resource and `ImageCacheItem::get()`.
    ///
    /// Measurement soundness notes (the naive approach silently measures
    /// nothing here):
    /// - the hit key is `black_box`ed per op, otherwise LLVM hoists the entire
    ///   loop-invariant hot hit (map lookup + front check never mutate) out of
    ///   the timing loop;
    /// - mid/cold batches replay a "self-sustaining cycle": repeatedly hitting
    ///   whatever entry currently sits at `depth` rotates the first depth+1
    ///   entries back to their start, so *every* timed op is a genuine
    ///   depth-`d` hit. Hitting the same key repeatedly would leave it at the
    ///   front after the first op and measure the fast path instead.
    struct LruMirror {
        usage_list: VecDeque<u64>,
        // 32-byte stand-in for `(ImageCacheItem, Resource)` map payloads
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

        /// Returns the depth the item was found at (0 = front / hot).
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

        /// The floor every hit path pays: map lookup without LRU bookkeeping.
        fn lookup_only(&self, hash: u64) -> bool {
            self.cache.contains_key(&hash)
        }

        /// Precomputes the op sequence whose every hit lands at exactly
        /// `depth`, leaving the LRU in its starting state once the sequence
        /// completes (asserted per op).
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

    /// Measured 2026-09-12 (release, median of 20 batches x 200 ops,
    /// 200 entries): bare map lookup 2.0 ns, hot/front hit 3.5 ns, depth-100
    /// hit 64.0 ns (+60.5 ns), depth-199 hit 90.0 ns (+86.5 ns). The front
    /// check keeps an LRU-hot hit within ~1.5 ns of the bare lookup floor,
    /// while a back-of-list hit pays ~26x the hot cost - the fast path removes
    /// ~96% of the hit cost for hot items, which covers the vast majority of
    /// real hits (the visible grid), so the guard in caching.rs is justified.
    #[test]
    #[ignore = "benchmark: run with --ignored"]
    fn bench_image_cache_lru_fast_path() {
        // usage_list depth = item index (front = 0). Depths mirror the task
        // spec: hot (front / fast path), mid-cache, and back-of-the-list cold.
        let cases: [(&str, usize); 3] = [
            ("hot  (front, fast path)", 0),
            ("warm (depth 100)        ", 100),
            ("cold (depth 199)        ", 199),
        ];

        let mut lru = LruMirror::new(ITEMS);
        let mut results = Vec::new();

        // lookup-only floor, measured with the same structure (opaque key)
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
            // stay aligned with the LRU's rotation state
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
    /// (`ui/library/playlist_view.rs:1315`) 在一次点击里做两件事，都在主线程：
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
    /// 再序列化落盘（`queue_manager/session.rs:11-13` 的注释承认这点，并用
    /// 500ms 前缘去抖把突发压成"每窗口一次快照"）。本机歌单只有 20 项，
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
