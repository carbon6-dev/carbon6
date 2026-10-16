//! Photos and videos: a watched folder's photos are read in place (EXIF
//! time, place, camera; a thumbnail), videos' containers give duration,
//! date and location; places come from a GeoNames file; photos close in
//! time become events (a trip, far from home); faces are grouped and named
//! in the review queue, joining the person you write to; a vision model
//! captions (search finds it); ffmpeg gives videos keyframes and a
//! transcription model their words.

use std::sync::Arc;
use std::time::Duration;

use c6_api::{MemexReview, MemexStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// The head's own knowledge store (docs/knowledge.md), and a folder for the test's files.
async fn memex_node(head: &c6_head::Head, _stop: &CancellationToken) -> (Arc<c6_memex::Memex>, tempfile::TempDir) {
    (c6_head::knowledge::get(&head.state).expect("the head's knowledge store"), tempfile::tempdir().unwrap())
}

/// A JPEG: a background, and a square in the middle (a "face" when red or
/// blue), with EXIF time, GPS and camera.
fn photo(bg: [u8; 3], center: [u8; 3], taken: &str, lat: f64, lon: f64) -> Vec<u8> {
    let img = image::RgbImage::from_fn(160, 120, |x, y| {
        if (60..100).contains(&x) && (40..80).contains(&y) { image::Rgb(center) } else { image::Rgb(bg) }
    });
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90).encode_image(&img).unwrap();
    let dms = |v: f64| {
        let v = v.abs();
        let (d, m) = (v.trunc(), (v.fract() * 60.0));
        exif::Value::Rational(vec![
            exif::Rational { num: d as u32, denom: 1 },
            exif::Rational { num: m.trunc() as u32, denom: 1 },
            exif::Rational { num: (m.fract() * 60.0 * 100.0).round() as u32, denom: 100 },
        ])
    };
    let ascii = |s: &str| exif::Value::Ascii(vec![s.as_bytes().to_vec()]);
    let fields = [
        exif::Field { tag: exif::Tag::Make, ifd_num: exif::In::PRIMARY, value: ascii("Canon") },
        exif::Field { tag: exif::Tag::Model, ifd_num: exif::In::PRIMARY, value: ascii("Canon EOS R6") },
        exif::Field { tag: exif::Tag::DateTimeOriginal, ifd_num: exif::In::PRIMARY, value: ascii(taken) },
        exif::Field { tag: exif::Tag::OffsetTimeOriginal, ifd_num: exif::In::PRIMARY, value: ascii("+00:00") },
        exif::Field { tag: exif::Tag::GPSLatitudeRef, ifd_num: exif::In::PRIMARY, value: ascii(if lat < 0.0 { "S" } else { "N" }) },
        exif::Field { tag: exif::Tag::GPSLatitude, ifd_num: exif::In::PRIMARY, value: dms(lat) },
        exif::Field { tag: exif::Tag::GPSLongitudeRef, ifd_num: exif::In::PRIMARY, value: ascii(if lon < 0.0 { "W" } else { "E" }) },
        exif::Field { tag: exif::Tag::GPSLongitude, ifd_num: exif::In::PRIMARY, value: dms(lon) },
    ];
    let mut w = exif::experimental::Writer::new();
    for f in &fields {
        w.push_field(f);
    }
    let mut tiff = std::io::Cursor::new(Vec::new());
    w.write(&mut tiff, false).unwrap();
    let tiff = tiff.into_inner();
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend(((tiff.len() + 8) as u16).to_be_bytes());
    out.extend(b"Exif\0\0");
    out.extend(&tiff);
    out.extend(&jpg[2..]);
    out
}

/// A QuickTime-style file: its duration, creation time and location.
fn video(seconds: u32, created_unix: i64, lat: f64, lon: f64) -> Vec<u8> {
    let bx = |kind: &[u8], body: Vec<u8>| -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(kind);
        b.extend(body);
        b
    };
    let mut mvhd = vec![0u8; 4];
    mvhd.extend(((created_unix + 2_082_844_800) as u32).to_be_bytes());
    mvhd.extend(0u32.to_be_bytes());
    mvhd.extend(1000u32.to_be_bytes());
    mvhd.extend((seconds * 1000).to_be_bytes());
    mvhd.extend([0u8; 80]);
    let loc = format!("{lat:+08.4}{lon:+09.4}/");
    let mut xyz = (loc.len() as u16).to_be_bytes().to_vec();
    xyz.extend(0x15c7u16.to_be_bytes());
    xyz.extend(loc.as_bytes());
    let moov = [bx(b"mvhd", mvhd), bx(b"udta", bx(&[0xA9, b'x', b'y', b'z'], xyz))].concat();
    [bx(b"ftyp", b"qt  \0\0\0\0qt  ".to_vec()), bx(b"mdat", vec![0; 64]), bx(b"moov", moov)].concat()
}

/// Finds the middle square when it is red or blue; describes a face by its
/// colour.
struct FakeFaces;

impl c6_extract::faces::FaceModel for FakeFaces {
    fn version(&self) -> String {
        "faces-v1:fake".into()
    }

    fn detect(&self, img: &image::RgbImage) -> anyhow::Result<Vec<c6_extract::faces::Found>> {
        let p = img.get_pixel(img.width() / 2, img.height() / 2);
        let face = (p[0] > 150 && p[2] < 100) || (p[2] > 150 && p[0] < 100);
        Ok(if face { vec![c6_extract::faces::Found { x: 0.375, y: 0.333, w: 0.25, h: 0.333, score: 0.99 }] } else { vec![] })
    }

    fn embed(&self, face: &image::RgbImage) -> anyhow::Result<Vec<f32>> {
        let p = face.get_pixel(face.width() / 2, face.height() / 2);
        Ok(c6_extract::faces::unit(vec![p[0] as f32, p[1] as f32, p[2] as f32]))
    }
}

#[tokio::test]
async fn photos_and_videos() {
    // The c6-extract kit's code, in this process, with face models that
    // find the coloured squares.
    c6_memex::extract::set_override(Arc::new(c6_memex::extract::InProcess { faces: Some(Arc::new(FakeFaces)) }));
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let stop = CancellationToken::new();
    assert!(
        http.post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let files = tempfile::tempdir().unwrap();
    let pics = files.path().join("Pictures");
    std::fs::create_dir_all(&pics).unwrap();

    // ffmpeg, as far as the memex can tell: keyframes are a sample picture,
    // the sound some bytes.
    let sample = files.path().join("sample.jpg");
    std::fs::write(&sample, photo([200, 180, 90], [200, 180, 90], "2023:06:10 12:00:00", 48.85, 2.35)).unwrap();
    let ffmpeg = files.path().join("ffmpeg");
    std::fs::write(
        &ffmpeg,
        format!("#!/bin/sh\nfor a; do out=\"$a\"; done\ncase \"$out\" in\n  *%02d.jpg) cp '{}' \"$(echo \"$out\" | sed 's/%02d/01/')\" ;;\n  *) printf 'sound' > \"$out\" ;;\nesac\n", sample.display()),
    )
    .unwrap();
    std::fs::set_permissions(&ffmpeg, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // SAFETY: set before the memex starts its work; nothing else reads it.
    unsafe { std::env::set_var("C6_FFMPEG", &ffmpeg) };

    // Places: home (Lisbon) and away (Paris).
    let places = files.path().join("cities.txt");
    std::fs::write(
        &places,
        "2267057\tLisbon\tLisbon\t\t38.71667\t-9.13333\tP\tPPLC\tPT\t\t14\t\t\t\t517802\t\t45\tEurope/Lisbon\t2022-01-01\n\
         2988507\tParis\tParis\t\t48.85341\t2.3488\tP\tPPLC\tFR\t\t11\t\t\t\t2138551\t\t42\tEurope/Paris\t2022-01-01\n",
    )
    .unwrap();

    // At home: a photo a week, three with the red-faced person.
    for i in 0..12 {
        let center = if i < 3 { [220, 30, 30] } else { [120, 120, 120] };
        let taken = format!("2023:{:02}:{:02} 10:00:00", 1 + i / 4, 1 + (i % 4) * 7);
        std::fs::write(pics.join(format!("home{i:02}.jpg")), photo([40, 120, 40], center, &taken, 38.72, -9.14)).unwrap();
    }
    // In Paris: ten photos over two days, one with the blue-faced person.
    for i in 0..10 {
        let center = if i == 4 { [30, 30, 220] } else { [120, 120, 120] };
        let hour = 8 + i * 3;
        let taken = format!("2023:06:{:02} {:02}:00:00", 10 + hour / 24, hour % 24);
        std::fs::write(pics.join(format!("paris{i:02}.jpg")), photo([90, 90, 160], center, &taken, 48.86, 2.34)).unwrap();
    }
    // And a video, the newest of all (11 June 2023, 14:00 UTC).
    std::fs::write(pics.join("clip.mov"), video(65, 1_686_492_000, 48.8566, 2.3522)).unwrap();
    // Someone you write to.

    let (m, _dir) = memex_node(&head, &stop).await;
    c6_memex::people::person(&m.store, "jane@acme.com", "Jane Doe").await.unwrap();
    // Face models (what the kit gets; the fake above stands in for them).
    for f in ["detector.onnx", "embedder.onnx"] {
        std::fs::write(files.path().join(f), b"onnx").unwrap();
    }
    let put = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"places_file": places.display().to_string(), "face_detector": files.path().join("detector.onnx"),
            "face_embedder": files.path().join("embedder.onnx")}))
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    m.set_places(Some(places.display().to_string())).await;
    let put = http
        .put(format!("{api}/settings/memex_folders"))
        .json(&json!([{"path": pics.display().to_string(), "labels": [], "enabled": true}]))
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success(), "{}", put.text().await.unwrap());
    // The new folder is scanned.
    eventually("the folder read", async || {
        let s: MemexStatus = http.get(format!("{api}/memex/status")).send().await.ok()?.json().await.ok()?;
        (s.media == 23).then_some(())
    })
    .await;

    let rpc = |method: &str, params: Value| {
        let (http, url, method) = (http.clone(), format!("{api}/memex/rpc"), method.to_owned());
        async move {
            let r = http.post(url).json(&json!({"method": method, "params": params})).send().await.unwrap();
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(ok, "{method}: {v}");
            v
        }
    };

    // Newest first: the video, then Paris.
    let all = rpc("memex.media.list", json!({})).await;
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 23);
    assert_eq!(all[0]["kind"], "video");
    assert_eq!(all[0]["duration"], 65.0);
    assert_eq!(all[0]["place"], "Paris, FR");
    assert_eq!(all[1]["title"], "paris09.jpg");
    let home = all.iter().find(|p| p["title"] == "home00.jpg").unwrap();
    assert_eq!(home["place"], "Lisbon, PT");
    let home_id = home["id"].as_str().unwrap().to_owned();
    let got = rpc("memex.media.get", json!({"id": home_id})).await;
    assert_eq!(got["camera"], "Canon EOS R6");
    assert_eq!(got["taken_at"], 1_672_567_200_000i64);
    assert!((got["lat"].as_f64().unwrap() - 38.72).abs() < 0.001);
    assert!(got["item"]["text"].as_str().unwrap().contains("in Lisbon, PT with Canon EOS R6"));
    // The thumbnail is a JPEG, through the head.
    let thumb = http.get(format!("{api}/memex/media/{home_id}/thumb")).send().await.unwrap();
    assert_eq!(thumb.headers()["content-type"], "image/jpeg");
    assert_eq!(&thumb.bytes().await.unwrap()[..2], &[0xFF, 0xD8]);

    // A trip: the Paris days, far from home.
    let facets = eventually("a trip", async || {
        let f = rpc("memex.media.facets", json!({})).await;
        f["events"].as_array()?.iter().any(|e| e["name"].as_str().unwrap_or("").starts_with("Trip: Paris, FR")).then_some(f)
    })
    .await;
    let trip = facets["events"].as_array().unwrap().iter().find(|e| e["name"].as_str().unwrap().starts_with("Trip")).unwrap();
    assert_eq!(trip["n"], 11, "ten photos and the video: {trip}");
    assert_eq!(facets["events"].as_array().unwrap().len(), 1, "home photos are a week apart: no events");
    let trip_photos = rpc("memex.media.list", json!({"about": trip["id"]})).await;
    assert_eq!(trip_photos.as_array().unwrap().len(), 11);

    // Faces: three of the red-faced person make a question; the blue one
    // (once) does not.
    let review: Vec<MemexReview> = eventually("a face to name", async || {
        let v: Vec<MemexReview> = serde_json::from_value(rpc("memex.review", json!({})).await).ok()?;
        v.iter().any(|r| r.kind == "face").then_some(v)
    })
    .await;
    let face = review.iter().find(|r| r.kind == "face").unwrap();
    assert_eq!(face.reason, "in 3 photos");
    let picture = http.get(format!("{api}/memex/{}", face.picture.as_ref().unwrap())).send().await.unwrap();
    assert!(picture.status().is_success());
    assert_eq!(&picture.bytes().await.unwrap()[..2], &[0xFF, 0xD8]);
    rpc("memex.review_decide", json!({"id": face.id, "accept": true, "name": "jane doe"})).await;
    let facets = rpc("memex.media.facets", json!({})).await;
    let people = facets["people"].as_array().unwrap();
    let jane = people.iter().find(|p| p["name"] == "Jane Doe").expect("Jane in the photos");
    assert_eq!((jane["n"].as_i64(), jane["label"].as_str()), (Some(3), Some("person:jane@acme.com")));
    assert!(people.iter().any(|p| p["name"].as_str().unwrap().starts_with("Unnamed person")));
    let janes = rpc("memex.media.list", json!({"about": "person:jane@acme.com"})).await;
    assert_eq!(janes.as_array().unwrap().len(), 3);
    let shown = rpc("memex.media.get", json!({"id": janes[0]["id"]})).await;
    assert_eq!(shown["faces"][0]["person"]["name"], "Jane Doe");
    let face_pic = http.get(format!("{api}/memex/face/person:jane@acme.com")).send().await.unwrap();
    assert!(face_pic.status().is_success());

    // The video got a keyframe (from "ffmpeg").
    let video_id = all[0]["id"].as_str().unwrap().to_owned();
    eventually("a keyframe", async || (rpc("memex.media.get", json!({"id": video_id})).await["frames"] == 1).then_some(())).await;

    // A vision model and a transcription model, for two calls today: the
    // newest (the video) is looked at and listened to.
    mock.push(Reply::Text(
        json!({"caption": "Children building a sandcastle by the Seine", "scene": "riverside", "objects": ["sandcastle", "bucket"], "text": ""})
            .to_string(),
    ));
    let put = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({
            "places_file": places.display().to_string(),
            "vision": {"connection": "m", "model": "eyes-1"},
            "transcription": {"connection": "m", "model": "ears-1"},
            "calls_per_day": 2,
        }))
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let item = eventually("a caption and a transcript", async || {
        let v = rpc("memex.media.get", json!({"id": video_id})).await;
        let text = v["item"]["text"].as_str()?.to_owned();
        (text.contains("sandcastle") && text.contains("Transcript:")).then_some(v)
    })
    .await;
    let text = item["item"]["text"].as_str().unwrap();
    assert!(text.contains("[0:00] hello\n[1:01] from paris"), "{text}");
    assert_eq!(item["item"]["title"], "Children building a sandcastle by the Seine");
    assert!(item["things"].as_array().unwrap().iter().any(|t| t["name"] == "sandcastle"));
    let asked = mock.requests()[0].to_string();
    assert!(asked.contains("image_url") && asked.contains("frames from one video"), "{asked}");
    let status: MemexStatus = http.get(format!("{api}/memex/status")).send().await.unwrap().json().await.unwrap();
    assert_eq!((status.media, status.media_seen, status.calls_today), (23, 1, 2));
    // Searching by description finds it.
    eventually("search by description", async || {
        let v = rpc("memex.media.list", json!({"q": "sandcastle"})).await;
        (v.as_array()?.first()?["id"] == video_id.as_str()).then_some(())
    })
    .await;

    // A photo deleted from the folder leaves the memex.
    std::fs::remove_file(pics.join("home11.jpg")).unwrap();
    http.post(format!("{api}/memex/scan")).send().await.unwrap();
    assert_eq!(rpc("memex.media.list", json!({})).await.as_array().unwrap().len(), 22);
    stop.cancel();
}
