//! Photos and videos: what the file says (EXIF time, place and camera; a
//! video's duration, date and location), a thumbnail, the place it was
//! taken (from an offline GeoNames file you give it), events and trips
//! (photos close in time), and then what a vision model sees (a caption,
//! the scene, objects), the faces in them (faces.rs), and what is said in
//! videos (a transcription model, with ffmpeg).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use c6_api as api;
use c6_llm::Block;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::{info, warn};

use crate::Memex;
use crate::llm::{self, Task};

pub const PHOTO_EXTENSIONS: [&str; 9] = ["jpg", "jpeg", "png", "heic", "heif", "webp", "tif", "tiff", "gif"];
pub const VIDEO_EXTENSIONS: [&str; 6] = ["mov", "mp4", "m4v", "3gp", "avi", "mkv"];
const THUMB: u32 = 640;
const VISION: &str = "vision-v1";
const HEARING: &str = "hear-v1";

pub fn kind_of(ext: &str) -> Option<&'static str> {
    if PHOTO_EXTENSIONS.contains(&ext) {
        Some("photo")
    } else if VIDEO_EXTENSIONS.contains(&ext) {
        Some("video")
    } else {
        None
    }
}

/// What a photo's EXIF says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exif {
    pub taken_at: Option<i64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub camera: String,
    pub orientation: u32,
}

pub fn read_exif(data: &[u8]) -> Exif {
    let mut out = Exif { orientation: 1, ..Default::default() };
    let Ok(e) = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(data)) else { return out };
    let ascii = |tag| match e.get_field(tag, exif::In::PRIMARY).map(|f| &f.value) {
        Some(exif::Value::Ascii(v)) => v.first().map(|s| String::from_utf8_lossy(s).trim().to_owned()),
        _ => None,
    };
    if let Some(f) =
        e.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY).or_else(|| e.get_field(exif::Tag::DateTime, exif::In::PRIMARY))
        && let exif::Value::Ascii(v) = &f.value
        && let Some(Ok(mut dt)) = v.first().map(|s| exif::DateTime::from_ascii(s))
    {
        if let Some(off) = ascii(exif::Tag::OffsetTimeOriginal) {
            let _ = dt.parse_offset(off.as_bytes());
        }
        out.taken_at = exif_time(&dt);
    }
    let coord = |tag, reference| -> Option<f64> {
        let exif::Value::Rational(r) = &e.get_field(tag, exif::In::PRIMARY)?.value else { return None };
        let v =
            r.first()?.to_f64() + r.get(1).map(|x| x.to_f64() / 60.0).unwrap_or(0.0) + r.get(2).map(|x| x.to_f64() / 3600.0).unwrap_or(0.0);
        let negative = ascii(reference).is_some_and(|s| s.starts_with('S') || s.starts_with('W'));
        v.is_finite().then_some(if negative { -v } else { v })
    };
    out.lat = coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef);
    out.lon = coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef);
    if out.lat == Some(0.0) && out.lon == Some(0.0) {
        (out.lat, out.lon) = (None, None);
    }
    let make = ascii(exif::Tag::Make).unwrap_or_default();
    let model = ascii(exif::Tag::Model).unwrap_or_default();
    out.camera = if model.starts_with(&make) { model } else { format!("{make} {model}").trim().to_owned() };
    if let Some(o) = e.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)) {
        out.orientation = o;
    }
    out
}

/// An EXIF time: with its offset if it has one, else in this machine's zone.
fn exif_time(dt: &exif::DateTime) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDate::from_ymd_opt(dt.year as i32, dt.month as u32, dt.day as u32)?.and_hms_opt(
        dt.hour as u32,
        dt.minute as u32,
        dt.second as u32,
    )?;
    let ms = match dt.offset {
        Some(off) => chrono::FixedOffset::east_opt(off as i32 * 60)?.from_local_datetime(&naive).single()?.timestamp_millis(),
        None => chrono::Local.from_local_datetime(&naive).earliest()?.timestamp_millis(),
    };
    (ms > 0).then_some(ms)
}

/// What a video's container says: (duration in seconds, created at, lat,
/// lon). MP4 and QuickTime only; others say nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoMeta {
    pub duration: Option<f64>,
    pub created_at: Option<i64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub fn read_video(path: &Path) -> VideoMeta {
    read_video_inner(path).unwrap_or_default()
}

fn read_video_inner(path: &Path) -> anyhow::Result<VideoMeta> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // The top-level boxes, to the moov box (often at the end).
    let mut pos = 0u64;
    let moov = loop {
        anyhow::ensure!(pos + 8 <= len, "no moov box");
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[..4].try_into()?) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into()?);
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        anyhow::ensure!(size >= header, "a broken box");
        if &h[4..8] == b"moov" {
            anyhow::ensure!(size < 64 * 1024 * 1024, "a huge moov box");
            let mut b = vec![0u8; (size - header) as usize];
            f.read_exact(&mut b)?;
            break b;
        }
        pos += size;
    };
    let mut out = VideoMeta::default();
    if let Some(mvhd) = find_box(&moov, b"mvhd") {
        let v = mvhd.first().copied().unwrap_or(0);
        let (created, scale, duration) = if v == 1 && mvhd.len() >= 32 {
            (
                u64::from_be_bytes(mvhd[4..12].try_into()?),
                u32::from_be_bytes(mvhd[20..24].try_into()?),
                u64::from_be_bytes(mvhd[24..32].try_into()?),
            )
        } else if mvhd.len() >= 20 {
            (
                u32::from_be_bytes(mvhd[4..8].try_into()?) as u64,
                u32::from_be_bytes(mvhd[12..16].try_into()?),
                u32::from_be_bytes(mvhd[16..20].try_into()?) as u64,
            )
        } else {
            (0, 0, 0)
        };
        if scale > 0 {
            out.duration = Some(duration as f64 / scale as f64);
        }
        // Seconds since 1904.
        const EPOCH_1904: i64 = 2_082_844_800;
        if created > EPOCH_1904 as u64 {
            out.created_at = Some((created as i64 - EPOCH_1904) * 1000);
        }
    }
    // A location, ISO 6709 (©xyz, or Apple's location key): +37.7749-122.4194+010.000/
    let text = String::from_utf8_lossy(&moov);
    let re = regex::Regex::new(r"([+-]\d{1,2}\.\d{2,})([+-]\d{1,3}\.\d{2,})").expect("regex");
    if let Some(c) = re.captures(&text) {
        out.lat = c[1].parse().ok();
        out.lon = c[2].parse().ok();
    }
    Ok(out)
}

/// The contents of the first box of this type, searching nested boxes.
fn find_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= data.len() {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().ok()?) as usize;
        if size < 8 || i + size > data.len() {
            return None;
        }
        let body = &data[i + 8..i + size];
        if &data[i + 4..i + 8] == kind {
            return Some(body);
        }
        if matches!(&data[i + 4..i + 8], b"trak" | b"mdia" | b"udta" | b"minf" | b"stbl" | b"meta")
            && let Some(b) = find_box(body, kind)
        {
            return Some(b);
        }
        i += size;
    }
    None
}

/// A JPEG thumbnail (up to `size` on its longer side, the right way up),
/// and the picture's size. HEIC goes through macOS's `sips` when there is
/// one.
pub async fn thumbnail(path: &Path, data: &[u8], orientation: u32, size: u32) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let img = decode(path, data).await?;
    let (data, w, h) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let img = orient(img, orientation);
        let (w, h) = (img.width(), img.height());
        Ok((jpeg(&img.thumbnail(size, size))?, w, h))
    })
    .await??;
    Ok((data, w, h))
}

/// The picture in a file.
pub async fn decode(path: &Path, data: &[u8]) -> anyhow::Result<image::DynamicImage> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if ext == "heic" || ext == "heif" {
        let out = std::env::temp_dir().join(format!("c6m-{}.jpg", uuid::Uuid::now_v7()));
        let status = tokio::process::Command::new("sips")
            .args(["-s", "format", "jpeg", "-Z", "2048"])
            .arg(path)
            .arg("--out")
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|_| anyhow::anyhow!("HEIC pictures need sips (macOS)"))?;
        anyhow::ensure!(status.success(), "sips could not read {}", path.display());
        let bytes = tokio::fs::read(&out).await;
        let _ = tokio::fs::remove_file(&out).await;
        let bytes = bytes?;
        return Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??);
    }
    let data = data.to_vec();
    Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&data)).await??)
}

pub fn orient(img: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

pub fn jpeg(img: &image::DynamicImage) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&img.to_rgb8())?;
    Ok(out)
}

/// Cities from a GeoNames file (cities1000.txt, cities15000.txt, …): the
/// nearest within 50 km names a place.
#[derive(Default)]
pub struct Places {
    pub file: String,
    cities: Vec<City>,
}

struct City {
    id: u64,
    lat: f64,
    lon: f64,
    name: String,
    country: String,
}

impl Places {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(c6_worker::expand_home(path))?;
        let mut cities = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 9 {
                continue;
            }
            let (Ok(id), Ok(lat), Ok(lon)) = (f[0].parse(), f[4].parse(), f[5].parse()) else { continue };
            cities.push(City { id, lat, lon, name: f[1].to_owned(), country: f[8].to_owned() });
        }
        anyhow::ensure!(!cities.is_empty(), "{path}: no places (expected a GeoNames file such as cities1000.txt)");
        Ok(Self { file: path.to_owned(), cities })
    }

    pub fn len(&self) -> usize {
        self.cities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cities.is_empty()
    }

    /// (key, name) of the nearest city, within 50 km.
    pub fn near(&self, lat: f64, lon: f64) -> Option<(String, String)> {
        let best = self.cities.iter().map(|c| (km(lat, lon, c.lat, c.lon), c)).min_by(|a, b| a.0.total_cmp(&b.0))?;
        (best.0 <= 50.0).then(|| (format!("place:geonames:{}", best.1.id), format!("{}, {}", best.1.name, best.1.country)))
    }
}

/// Great-circle distance.
pub fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%-d %B %Y").to_string()).unwrap_or_default()
}

/// Adds (or updates) a photo or video from a watched folder. Its id.
pub async fn add(m: &Memex, path: &Path, kind: &str, mtime: i64, size: u64, labels: &[String]) -> anyhow::Result<String> {
    let p = path.display().to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone());
    let mut meta = json!({"name": name, "path": p, "size": size, "mime": mime(&name)});
    let (taken_at, lat, lon, camera, duration);
    let (mut thumb, mut dims, mut error) = (None, None, None);
    if kind == "photo" {
        anyhow::ensure!(size <= 200 * 1024 * 1024, "too large for a photo");
        let data = tokio::fs::read(path).await?;
        let e = read_exif(&data);
        (taken_at, lat, lon, camera, duration) = (e.taken_at, e.lat, e.lon, e.camera.clone(), None);
        match thumbnail(path, &data, e.orientation, THUMB).await {
            Ok((jpg, w, h)) => {
                thumb = Some(m.put_blob(&jpg).await?);
                dims = Some((w, h));
            }
            Err(e) => error = Some(format!("no thumbnail: {e:#}")),
        }
    } else {
        let pb = path.to_owned();
        let v = tokio::task::spawn_blocking(move || read_video(&pb)).await?;
        (taken_at, lat, lon, camera, duration) = (v.created_at, v.lat, v.lon, String::new(), v.duration);
    }
    let when = taken_at.unwrap_or(mtime);
    let place = match (lat, lon) {
        (Some(la), Some(lo)) => {
            let near = m.places.lock().unwrap().as_ref().and_then(|p| p.near(la, lo));
            match near {
                Some((key, name)) => Some((m.store.ensure_thing(&key, "place", &name).await?, name)),
                None => None,
            }
        }
        _ => None,
    };
    if let (Some(la), Some(lo)) = (lat, lon) {
        meta["lat"] = json!(la);
        meta["lon"] = json!(lo);
    }
    if !camera.is_empty() {
        meta["camera"] = json!(camera);
    }
    if let Some(d) = duration {
        meta["duration"] = json!(d);
    }
    if let Some((w, h)) = dims {
        meta["width"] = json!(w);
        meta["height"] = json!(h);
    }
    let what = if kind == "photo" { "Photo" } else { "Video" };
    let mut text = format!("{what} taken {}", date(when));
    if let Some((_, n)) = &place {
        text.push_str(&format!(" in {n}"));
    }
    if !camera.is_empty() {
        text.push_str(&format!(" with {camera}"));
    }
    if let Some(d) = duration {
        text.push_str(&format!(" ({}:{:02})", d as u64 / 60, d as u64 % 60));
    }
    text.push('.');
    // What has been seen and heard stays, unless the file changed.
    let old = sqlx::query("SELECT md.*, i.title, i.body FROM media md JOIN items i ON i.id = md.item_id WHERE md.path = ?")
        .bind(&p)
        .fetch_optional(&m.store.pool)
        .await?;
    let item = api::MemexItemIn {
        kind: kind.into(),
        source: "folder".into(),
        external_id: p.clone(),
        title: name.clone(),
        text: text.clone(),
        created_at: Some(when),
        labels: labels.to_vec(),
        meta,
        ..Default::default()
    };
    let id = m.store.put(&item, "folder").await?;
    if old.is_some() {
        sqlx::query("DELETE FROM faces WHERE item_id = ?").bind(&id).execute(&m.store.pool).await?;
        sqlx::query("DELETE FROM links WHERE src_item = ? AND basis = 'inferred'").bind(&id).execute(&m.store.pool).await?;
    }
    m.store.clear_links(&id, "media").await?;
    if let Some((place_id, _)) = &place {
        m.store.link(&id, "at", place_id, "observed", "media", None).await?;
    }
    sqlx::query(
        "INSERT OR REPLACE INTO media (item_id, path, kind, taken_at, lat, lon, place, camera, width, height, duration, thumb, error) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&p)
    .bind(kind)
    .bind(when)
    .bind(lat)
    .bind(lon)
    .bind(place.as_ref().map(|p| p.0.clone()))
    .bind(&camera)
    .bind(dims.map(|d| d.0 as i64))
    .bind(dims.map(|d| d.1 as i64))
    .bind(duration)
    .bind(&thumb)
    .bind(&error)
    .execute(&m.store.pool)
    .await?;
    m.media_changed.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(id)
}

fn mime(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "heic" | "heif" => "image/heic",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "3gp" => "video/3gpp",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    }
}

/// ffmpeg, if this machine has it (or `C6_FFMPEG` says where).
pub fn ffmpeg() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("C6_FFMPEG") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

/// Places photos and videos already in the memex (a places file arrived).
pub async fn place_all(m: &Memex) -> anyhow::Result<u64> {
    let rows = sqlx::query("SELECT item_id, lat, lon FROM media WHERE lat IS NOT NULL AND place IS NULL").fetch_all(&m.store.pool).await?;
    let mut n = 0;
    for r in rows {
        let near = m.places.lock().unwrap().as_ref().and_then(|p| p.near(r.get("lat"), r.get("lon")));
        if let Some((key, name)) = near {
            let id: String = r.get("item_id");
            let place = m.store.ensure_thing(&key, "place", &name).await?;
            m.store.link(&id, "at", &place, "observed", "media", None).await?;
            sqlx::query("UPDATE media SET place = ? WHERE item_id = ?").bind(&place).bind(&id).execute(&m.store.pool).await?;
            n += 1;
        }
    }
    if n > 0 {
        m.media_changed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(n)
}

/// Events from photos and videos close in time (gaps under 6 hours), ten
/// or more; a trip when one spans days far (100+ km) from where most are
/// taken. Inferred, and made again when media change.
pub async fn events(m: &Memex) -> anyhow::Result<usize> {
    const MADE_BY: &str = "media-events-v1";
    let rows = sqlx::query("SELECT item_id, taken_at, lat, lon, place FROM media WHERE taken_at IS NOT NULL ORDER BY taken_at")
        .fetch_all(&m.store.pool)
        .await?;
    // Home: the most common place, roughly (0.5° cells).
    let mut cells: std::collections::HashMap<(i64, i64), (u32, f64, f64)> = Default::default();
    for r in &rows {
        if let (Some(la), Some(lo)) = (r.get::<Option<f64>, _>("lat"), r.get::<Option<f64>, _>("lon")) {
            let e = cells.entry(((la * 2.0).round() as i64, (lo * 2.0).round() as i64)).or_insert((0, la, lo));
            e.0 += 1;
        }
    }
    let home = cells.values().max_by_key(|c| c.0).map(|c| (c.1, c.2));
    let mut groups: Vec<Vec<&sqlx::sqlite::SqliteRow>> = Vec::new();
    for r in &rows {
        let t: i64 = r.get("taken_at");
        match groups.last_mut() {
            Some(g) if t - g.last().map(|x| x.get::<i64, _>("taken_at")).unwrap_or(t) < 6 * 3_600_000 => g.push(r),
            _ => groups.push(vec![r]),
        }
    }
    let mut tx = m.store.pool.begin().await?;
    sqlx::query("DELETE FROM evidence WHERE link_id IN (SELECT id FROM links WHERE made_by = ?)").bind(MADE_BY).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM links WHERE made_by = ?").bind(MADE_BY).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut made = 0;
    for g in groups.iter().filter(|g| g.len() >= 10) {
        let first: String = g[0].get("item_id");
        let (start, end): (i64, i64) = (g[0].get("taken_at"), g[g.len() - 1].get("taken_at"));
        // The most common place in the group.
        let mut places: std::collections::HashMap<String, u32> = Default::default();
        for r in g {
            if let Some(p) = r.get::<Option<String>, _>("place") {
                *places.entry(p).or_default() += 1;
            }
        }
        let place = places.into_iter().max_by_key(|p| p.1).map(|p| p.0);
        let place_name = match &place {
            Some(p) => m.store.thing(p).await?.map(|t| t.name),
            None => None,
        };
        let far = home.is_some_and(|(hl, ho)| {
            g.iter().any(|r| match (r.get::<Option<f64>, _>("lat"), r.get::<Option<f64>, _>("lon")) {
                (Some(la), Some(lo)) => km(hl, ho, la, lo) > 100.0,
                _ => false,
            })
        });
        let days = (end - start) > 20 * 3_600_000;
        let span = if days { format!("{} – {}", date(start), date(end)) } else { date(start) };
        let name = match (far && days, &place_name) {
            (true, Some(p)) => format!("Trip: {p}, {span}"),
            (true, None) => format!("Trip, {span}"),
            (false, Some(p)) => format!("{p}, {span}"),
            (false, None) => format!("Photos, {span}"),
        };
        let event = m.store.ensure_thing(&format!("event:photos:{first}"), "event", &name).await?;
        sqlx::query("UPDATE things SET name = ? WHERE id = ?").bind(&name).bind(&event).execute(&m.store.pool).await?;
        for r in g {
            m.store.link_inferred(&r.get::<String, _>("item_id"), "part_of", &event, 0.8, MADE_BY, None).await?;
        }
        if let Some(p) = &place {
            m.store.link_things(&event, "at", p, "inferred", MADE_BY).await?;
        }
        made += 1;
    }
    // Events no longer made (their photos moved apart, or went).
    sqlx::query(
        "DELETE FROM things WHERE key LIKE 'event:photos:%' AND NOT EXISTS (SELECT 1 FROM links l WHERE l.dst_thing = things.id) \
         AND NOT EXISTS (SELECT 1 FROM trail_steps s WHERE s.target = things.id)",
    )
    .execute(&m.store.pool)
    .await?;
    sqlx::query("DELETE FROM links WHERE made_by = ? AND src_thing IS NOT NULL AND src_thing NOT IN (SELECT id FROM things)")
        .bind(MADE_BY)
        .execute(&m.store.pool)
        .await?;
    Ok(made)
}

const LOOK: &str = "Describe this for a private photo archive, so it can be found by search later. JSON only: {\"caption\": one \
     plain sentence, \"scene\": a few words (e.g. beach at sunset, office meeting), \"objects\": up to 8 short nouns of what is \
     visible, \"text\": legible text in it, or \"\"}. Do not guess who people are.";

/// Has the vision model look at one more photo or video. Whether it did.
pub async fn look(m: &Memex) -> anyhow::Result<bool> {
    let t = m.tasks();
    let Some(task) = t.vision else { return Ok(false) };
    if crate::understand::budget_left(m, t.per_day).await? == 0 {
        return Ok(false);
    }
    let version = format!("{VISION}@{}", task.model);
    let Some(r) = sqlx::query(
        "SELECT md.*, i.title, i.body FROM media md JOIN items i ON i.id = md.item_id WHERE (md.seen IS NULL OR md.seen != ?) \
         AND (md.thumb IS NOT NULL OR md.frames != '[]') ORDER BY md.taken_at DESC LIMIT 1",
    )
    .bind(&version)
    .fetch_optional(&m.store.pool)
    .await?
    else {
        return Ok(false);
    };
    crate::understand::count_call(m).await?;
    let id: String = r.get("item_id");
    let result = see(m, &task, &version, &r).await;
    if let Err(e) = &result {
        warn!(item = %id, "vision: {e:#}");
    }
    sqlx::query("UPDATE media SET seen = ?, error = ? WHERE item_id = ?")
        .bind(&version)
        .bind(result.err().map(|e| format!("vision: {e:#}")))
        .bind(&id)
        .execute(&m.store.pool)
        .await?;
    Ok(true)
}

async fn see(m: &Memex, task: &Task, version: &str, r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<()> {
    let id: String = r.get("item_id");
    let frames: Vec<String> = serde_json::from_str(&r.get::<String, _>("frames")).unwrap_or_default();
    let pictures: Vec<String> =
        if frames.is_empty() { r.get::<Option<String>, _>("thumb").into_iter().collect() } else { frames.into_iter().take(4).collect() };
    let mut blocks = Vec::new();
    for p in &pictures {
        blocks.push(Block::Image { media_type: "image/jpeg".into(), data: tokio::fs::read(m.blob_path(p)).await? });
    }
    let intro = if r.get::<String, _>("kind") == "video" { "These are frames from one video, in order. " } else { "" };
    blocks.push(Block::Text(format!("{intro}{LOOK}")));
    let answer = llm::ask(&m.link, task, "You describe pictures faithfully and briefly. Answer with one JSON object.", blocks, 600).await?;
    let v = llm::json_of(&answer)?;
    let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_owned();
    let (caption, scene, seen_text) = (s("caption"), s("scene"), s("text"));
    let objects: Vec<String> = v["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o.as_str())
        .map(|o| o.trim().to_lowercase())
        .filter(|o| !o.is_empty() && o.len() < 40)
        .take(8)
        .collect();
    // The facts line from the file stays first.
    let facts = r.get::<String, _>("body").lines().next().unwrap_or("").to_owned();
    let mut text = facts;
    for (label, part) in [("", caption.as_str()), ("Scene: ", scene.as_str()), ("Text in it: ", seen_text.as_str())] {
        if !part.is_empty() {
            text.push_str(&format!("\n{label}{part}"));
        }
    }
    if !objects.is_empty() {
        text.push_str(&format!("\nShows: {}", objects.join(", ")));
    }
    let transcript = r.get::<String, _>("body").split_once("\nTranscript:\n").map(|(_, t)| t.to_owned());
    if let Some(t) = transcript {
        text.push_str(&format!("\nTranscript:\n{t}"));
    }
    let title = if caption.is_empty() { r.get::<String, _>("title") } else { crate::store::clip(&caption, 100) };
    m.store.set_text(&id, &title, &text).await?;
    sqlx::query("DELETE FROM links WHERE src_item = ? AND made_by LIKE 'vision-%'").bind(&id).execute(&m.store.pool).await?;
    for o in objects {
        let slug: String = o.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let thing = m.store.ensure_thing(&format!("object:{slug}"), "object", &o).await?;
        m.store.link_inferred(&id, "shows", &thing, 0.7, version, None).await?;
    }
    Ok(())
}

/// Keyframes and a transcript for one more video (with ffmpeg). Whether it
/// did.
pub async fn hear(m: &Memex) -> anyhow::Result<bool> {
    let Some(ffmpeg) = ffmpeg() else { return Ok(false) };
    let task = m.tasks().transcription;
    let version = format!("{HEARING}@{}", task.as_ref().map(|t| t.model.as_str()).unwrap_or("-"));
    let Some(r) = sqlx::query("SELECT md.*, i.title, i.body FROM media md JOIN items i ON i.id = md.item_id WHERE md.kind = 'video' AND (md.heard IS NULL OR md.heard != ?) ORDER BY md.taken_at DESC LIMIT 1")
        .bind(&version)
        .fetch_optional(&m.store.pool)
        .await?
    else {
        return Ok(false);
    };
    let id: String = r.get("item_id");
    let path = PathBuf::from(r.get::<String, _>("path"));
    let dir = std::env::temp_dir().join(format!("c6m-video-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&dir).await?;
    let result = async {
        // Keyframes (once): the first frame and scene changes, up to 6.
        if r.get::<String, _>("frames") == "[]" {
            let status = tokio::process::Command::new(&ffmpeg)
                .args(["-nostdin", "-loglevel", "error", "-i"])
                .arg(&path)
                .args(["-vf", "select='eq(n\\,0)+gt(scene\\,0.35)',scale=640:-2", "-fps_mode", "vfr", "-frames:v", "6", "-q:v", "4"])
                .arg(dir.join("f%02d.jpg"))
                .status()
                .await?;
            anyhow::ensure!(status.success(), "ffmpeg could not read the video");
            let mut frames = Vec::new();
            for i in 1..=6 {
                if let Ok(b) = tokio::fs::read(dir.join(format!("f{i:02}.jpg"))).await {
                    frames.push(m.put_blob(&b).await?);
                }
            }
            // New frames want looking at.
            sqlx::query("UPDATE media SET frames = ?, thumb = COALESCE(thumb, ?), seen = NULL WHERE item_id = ?")
                .bind(serde_json::to_string(&frames)?)
                .bind(frames.first())
                .bind(&id)
                .execute(&m.store.pool)
                .await?;
        }
        let Some(task) = &task else { return Ok(()) };
        let budget = m.tasks().per_day;
        if crate::understand::budget_left(m, budget).await? == 0 {
            anyhow::bail!("budget");
        }
        let audio = dir.join("a.m4a");
        let status = tokio::process::Command::new(&ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "aac", "-b:a", "32k", "-t", "5400"])
            .arg(&audio)
            .status()
            .await?;
        if !status.success() || !audio.exists() {
            // No sound.
            return Ok(());
        }
        crate::understand::count_call(m).await?;
        let transcript = transcribe(m, task, &tokio::fs::read(&audio).await?).await?;
        if !transcript.trim().is_empty() {
            let body: String = r.get("body");
            let base = body.split_once("\nTranscript:\n").map(|(b, _)| b.to_owned()).unwrap_or(body);
            m.store.set_text(&id, &r.get::<String, _>("title"), &format!("{base}\nTranscript:\n{transcript}")).await?;
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    match result {
        Err(e) if e.to_string() == "budget" => Ok(false),
        r => {
            let error = r.err().map(|e| format!("video: {e:#}"));
            if let Some(e) = &error {
                warn!(item = %id, "{e}");
            }
            sqlx::query("UPDATE media SET heard = ?, error = COALESCE(?, error) WHERE item_id = ?")
                .bind(&version)
                .bind(&error)
                .bind(&id)
                .execute(&m.store.pool)
                .await?;
            Ok(true)
        }
    }
}

/// Speech to text, with timestamps: an OpenAI-style /audio/transcriptions.
pub async fn transcribe(m: &Memex, task: &Task, audio: &[u8]) -> anyhow::Result<String> {
    let grant = m.link.credential(&task.connection_id).await?;
    let url = format!("{}/audio/transcriptions", grant.base_url.trim_end_matches('/'));
    let form = reqwest::multipart::Form::new()
        .text("model", task.model.clone())
        .text("response_format", "verbose_json")
        .part("file", reqwest::multipart::Part::bytes(audio.to_vec()).file_name("audio.m4a").mime_str("audio/mp4")?);
    let mut req = reqwest::Client::new().post(&url).bearer_auth(&grant.bearer).multipart(form);
    for (k, v) in &grant.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    anyhow::ensure!(status.is_success(), "transcription: {status} {}", crate::store::clip(&text, 300));
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"text": text}));
    let segments = v["segments"].as_array().cloned().unwrap_or_default();
    if segments.is_empty() {
        return Ok(v["text"].as_str().unwrap_or("").trim().to_owned());
    }
    Ok(segments
        .iter()
        .map(|s| {
            let t = s["start"].as_f64().unwrap_or(0.0) as u64;
            format!("[{}:{:02}] {}", t / 60, t % 60, s["text"].as_str().unwrap_or("").trim())
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Looks at, listens to and finds faces in media in the background, and
/// makes events when media change.
pub fn spawn(m: Arc<Memex>) {
    tokio::spawn(async move {
        let mut events_at: Option<std::time::Instant> = None;
        loop {
            let mut worked = false;
            if m.link.is_connected() {
                for (what, r) in [("vision", look(&m).await), ("faces", crate::faces::step(&m).await), ("video", hear(&m).await)] {
                    match r {
                        Ok(w) => worked |= w,
                        Err(e) => warn!("{what}: {e:#}"),
                    }
                }
            }
            if m.media_changed.load(std::sync::atomic::Ordering::Relaxed)
                && !m.scanning.load(std::sync::atomic::Ordering::Relaxed)
                && events_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(30))
            {
                m.media_changed.store(false, std::sync::atomic::Ordering::Relaxed);
                match events(&m).await {
                    Ok(n) if n > 0 => info!(events = n, "events from photos"),
                    Ok(_) => {}
                    Err(e) => warn!("events from photos: {e:#}"),
                }
                events_at = Some(std::time::Instant::now());
            }
            if worked {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            } else {
                tokio::select! {
                    _ = m.wake.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                }
            }
        }
    });
}

/// `memex.media.list`: photos and videos, newest first; by search, person,
/// place or event, and before a time (paging).
pub async fn list(m: &Memex, params: &Value, allowed: &[String]) -> anyhow::Result<Value> {
    let limit = params["limit"].as_u64().unwrap_or(120).min(500) as i64;
    let before = params["before"].as_i64().unwrap_or(i64::MAX);
    let kinds: Vec<String> = ["photo", "video"].iter().filter(|k| allowed.iter().any(|a| a == *k)).map(|k| k.to_string()).collect();
    anyhow::ensure!(!kinds.is_empty(), "photos are outside what this caller may see");
    let mut ids: Option<Vec<String>> = None;
    if let Some(q) = params["q"].as_str().filter(|q| !q.trim().is_empty()) {
        let hits = m
            .search(
                &api::MemexQuery { query: q.to_owned(), kinds: kinds.clone(), limit: Some(limit as u32), ..Default::default() },
                allowed,
            )
            .await?;
        ids = Some(hits.into_iter().map(|h| h.id).collect());
    }
    let about = params["about"].as_str().filter(|a| !a.is_empty());
    let about_id = match about {
        Some(a) => Some(m.store.thing(a).await?.ok_or_else(|| anyhow::anyhow!("no thing {a:?}"))?.id),
        None => None,
    };
    let rows = sqlx::query(
        "SELECT md.*, i.title, i.created_at, p.name AS place_name FROM media md JOIN items i ON i.id = md.item_id LEFT JOIN things p ON p.id = md.place \
         WHERE i.kind IN (SELECT value FROM json_each(?1)) AND md.taken_at < ?2 \
         AND (?3 IS NULL OR md.item_id IN (SELECT value FROM json_each(?3))) \
         AND (?4 IS NULL OR EXISTS (SELECT 1 FROM links l WHERE l.src_item = md.item_id AND l.dst_thing = ?4)) \
         ORDER BY CASE WHEN ?3 IS NULL THEN 0 ELSE 1 END, md.taken_at DESC LIMIT ?5",
    )
    .bind(serde_json::to_string(&kinds)?)
    .bind(before)
    .bind(ids.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&about_id)
    .bind(limit)
    .fetch_all(&m.store.pool)
    .await?;
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("item_id"), "kind": r.get::<String, _>("kind"), "title": r.get::<String, _>("title"),
                "taken_at": r.get::<Option<i64>, _>("taken_at"), "place": r.get::<Option<String>, _>("place_name"),
                "thumb": r.get::<Option<String>, _>("thumb").is_some(), "duration": r.get::<Option<f64>, _>("duration"),
            })
        })
        .collect();
    // In search order, when searching.
    if let Some(ids) = &ids {
        out.sort_by_key(|v| ids.iter().position(|i| v["id"] == i.as_str()).unwrap_or(usize::MAX));
    }
    Ok(json!(out))
}

/// `memex.media.get`: a photo or video with its faces, place and things.
pub async fn get(m: &Memex, id: &str, allowed: &[String]) -> anyhow::Result<Value> {
    let item = m.store.item(id, allowed).await?.ok_or_else(|| anyhow::anyhow!("no photo {id:?}"))?;
    let r = sqlx::query("SELECT * FROM media WHERE item_id = ?")
        .bind(id)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{id} is not a photo or video"))?;
    let faces = sqlx::query("SELECT f.id, f.x, f.y, f.w, f.h, t.id AS tid, t.name, t.key FROM faces f LEFT JOIN things t ON t.id = f.person WHERE f.item_id = ?")
        .bind(id)
        .fetch_all(&m.store.pool)
        .await?;
    let shows = sqlx::query("SELECT t.* FROM links l JOIN things t ON t.id = l.dst_thing WHERE l.src_item = ? AND l.rel IN ('shows', 'part_of', 'at', 'depicts') GROUP BY t.id")
        .bind(id)
        .fetch_all(&m.store.pool)
        .await?;
    let frames: Vec<String> = serde_json::from_str(&r.get::<String, _>("frames")).unwrap_or_default();
    Ok(json!({
        "item": item,
        "taken_at": r.get::<Option<i64>, _>("taken_at"),
        "lat": r.get::<Option<f64>, _>("lat"), "lon": r.get::<Option<f64>, _>("lon"),
        "camera": r.get::<String, _>("camera"), "duration": r.get::<Option<f64>, _>("duration"),
        "frames": frames.len(),
        "error": r.get::<Option<String>, _>("error"),
        "faces": faces.iter().map(|f| json!({
            "id": f.get::<i64, _>("id"), "x": f.get::<f64, _>("x"), "y": f.get::<f64, _>("y"), "w": f.get::<f64, _>("w"), "h": f.get::<f64, _>("h"),
            "person": f.get::<Option<String>, _>("tid").map(|t| json!({"id": t, "name": f.get::<Option<String>, _>("name"), "label": f.get::<Option<String>, _>("key")})),
        })).collect::<Vec<_>>(),
        "things": shows.iter().map(|t| json!({"id": t.get::<String, _>("id"), "kind": t.get::<String, _>("kind"), "name": t.get::<String, _>("name"), "label": t.get::<Option<String>, _>("key")})).collect::<Vec<_>>(),
    }))
}

/// `memex.media.thumb`: a thumbnail (or a video's nth keyframe), as JPEG.
pub async fn thumb(m: &Memex, id: &str, frame: Option<usize>) -> anyhow::Result<Vec<u8>> {
    let r = sqlx::query("SELECT thumb, frames FROM media WHERE item_id = ?")
        .bind(id)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no photo {id:?}"))?;
    let hash = match frame {
        Some(n) => serde_json::from_str::<Vec<String>>(&r.get::<String, _>("frames")).unwrap_or_default().get(n).cloned(),
        None => r.get("thumb"),
    }
    .ok_or_else(|| anyhow::anyhow!("no picture for {id}"))?;
    Ok(tokio::fs::read(m.blob_path(&hash)).await?)
}

/// `memex.media.people` and places: who and where appear most.
pub async fn facets(m: &Memex) -> anyhow::Result<Value> {
    let q = |rel: &'static str| async move {
        sqlx::query(
            "SELECT t.id, t.name, t.key, t.kind, COUNT(DISTINCT l.src_item) AS n FROM links l JOIN things t ON t.id = l.dst_thing \
             JOIN media md ON md.item_id = l.src_item WHERE l.rel = ? GROUP BY t.id ORDER BY n DESC LIMIT 40",
        )
        .bind(rel)
        .fetch_all(&m.store.pool)
        .await
    };
    let row = |r: &sqlx::sqlite::SqliteRow| json!({"id": r.get::<String, _>("id"), "name": r.get::<String, _>("name"), "label": r.get::<Option<String>, _>("key"), "n": r.get::<i64, _>("n")});
    Ok(json!({
        "people": q("depicts").await?.iter().map(row).collect::<Vec<_>>(),
        "places": q("at").await?.iter().map(row).collect::<Vec<_>>(),
        "events": q("part_of").await?.iter().map(row).collect::<Vec<_>>(),
    }))
}

pub async fn status(m: &Memex, s: &mut api::MemexStatus) -> anyhow::Result<()> {
    s.media = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media").fetch_one(&m.store.pool).await? as u64;
    s.media_seen = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media WHERE seen IS NOT NULL").fetch_one(&m.store.pool).await? as u64;
    Ok(())
}
