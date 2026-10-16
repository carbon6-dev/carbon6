//! Photos and videos: what the file says (EXIF time, place, camera and
//! orientation; a video's duration, date and location), a thumbnail, and a
//! video's keyframes and sound (with ffmpeg).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Value, json};

/// Thumbnails' longer side.
const THUMB: u32 = 640;

/// What a photo's EXIF says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exif {
    /// When, as the camera's clock said it ("2024-05-01T10:00:00"), and its
    /// offset from UTC in minutes if it gave one: the knowledge store reads
    /// a time without one in its own zone.
    pub taken: Option<String>,
    pub offset: Option<i16>,
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
        out.taken = Some(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second));
        out.offset = dt.offset;
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
/// and the picture's size.
pub fn thumbnail(path: &Path, data: &[u8], orientation: u32, size: u32) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let img = orient(decode(path, data)?, orientation);
    let (w, h) = (img.width(), img.height());
    Ok((jpeg(&img.thumbnail(size, size))?, w, h))
}

/// The picture in a file. HEIC goes through heif-convert (libheif) or
/// macOS's `sips`, whichever there is.
pub fn decode(path: &Path, data: &[u8]) -> anyhow::Result<image::DynamicImage> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if ext == "heic" || ext == "heif" {
        let out = std::env::temp_dir().join(format!("c6x-{}.jpg", std::process::id()));
        let quiet = |c: &mut std::process::Command| c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let ok = quiet(std::process::Command::new("heif-convert").arg(path).arg(&out)).is_ok_and(|s| s.success())
            || quiet(std::process::Command::new("sips").args(["-s", "format", "jpeg", "-Z", "2048"]).arg(path).arg("--out").arg(&out))
                .is_ok_and(|s| s.success());
        anyhow::ensure!(ok, "could not read the HEIC picture {}", path.display());
        let bytes = std::fs::read(&out);
        let _ = std::fs::remove_file(&out);
        return Ok(image::load_from_memory(&bytes?)?);
    }
    Ok(image::load_from_memory(data)?)
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

/// What the knowledge store keeps of a photo or video, and a photo's
/// thumbnail.
pub fn media(path: &Path) -> anyhow::Result<(Value, Option<Vec<u8>>)> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        let v = read_video(path);
        return Ok((json!({"kind": "video", "duration": v.duration, "created_at": v.created_at, "lat": v.lat, "lon": v.lon}), None));
    }
    let size = std::fs::metadata(path)?.len();
    anyhow::ensure!(size <= 200 * 1024 * 1024, "too large for a photo");
    let data = std::fs::read(path)?;
    let e = read_exif(&data);
    let mut v = json!({"kind": "photo", "taken": e.taken, "offset": e.offset, "lat": e.lat, "lon": e.lon, "camera": e.camera,
        "orientation": e.orientation});
    match thumbnail(path, &data, e.orientation, THUMB) {
        Ok((jpg, w, h)) => {
            v["width"] = json!(w);
            v["height"] = json!(h);
            Ok((v, Some(jpg)))
        }
        Err(err) => {
            v["error"] = json!(format!("no thumbnail: {err:#}"));
            Ok((v, None))
        }
    }
}

pub const VIDEO_EXTENSIONS: [&str; 6] = ["mov", "mp4", "m4v", "3gp", "avi", "mkv"];

/// ffmpeg, if this machine has it (or `C6_FFMPEG` says where).
pub fn ffmpeg() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("C6_FFMPEG") {
        return Some(p.into());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(["/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"].map(std::path::PathBuf::from))
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

/// A video's keyframes (the first frame and scene changes, up to 6, as
/// f01.jpg …) and, if asked, its sound (a.m4a, mono, up to 90 minutes),
/// into `dir`. Whether it has sound.
pub fn video(path: &Path, dir: &Path, sound: bool) -> anyhow::Result<bool> {
    let ffmpeg = ffmpeg().ok_or_else(|| anyhow::anyhow!("no ffmpeg"))?;
    let status = std::process::Command::new(&ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-vf", "select='eq(n\\,0)+gt(scene\\,0.35)',scale=640:-2", "-fps_mode", "vfr", "-frames:v", "6", "-q:v", "4"])
        .arg(dir.join("f%02d.jpg"))
        .status()?;
    anyhow::ensure!(status.success(), "ffmpeg could not read the video");
    if !sound {
        return Ok(false);
    }
    let audio = dir.join("a.m4a");
    let status = std::process::Command::new(&ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "aac", "-b:a", "32k", "-t", "5400"])
        .arg(&audio)
        .status()?;
    Ok(status.success() && audio.exists())
}
