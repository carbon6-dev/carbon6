//! Faces in photos: found by a detector, described by an embedder (ONNX
//! models you give the memex: e.g. UltraFace and ArcFace or MobileFaceNet),
//! and grouped: a face like the faces of someone (closer than THRESHOLD to
//! their average) is theirs; otherwise it starts a new, unnamed person.
//! Once someone has a few photos, naming them is in the review queue, and
//! naming joins them to the person you mean (the one you email, say).

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::Row;
use tracing::info;

use crate::Memex;

/// How alike two faces' embeddings must be (cosine) to be one person.
const THRESHOLD: f32 = 0.45;
/// Faces before an unnamed person is put to you.
const ASK_AFTER: i64 = 3;
/// Photos per step.
const BATCH: i64 = 8;

/// A face: where (0..1 of the picture) and how sure.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

pub trait FaceModel: Send + Sync {
    /// Which models (stored with what they found).
    fn version(&self) -> String;
    fn detect(&self, img: &image::RgbImage) -> anyhow::Result<Vec<Found>>;
    /// A face (cropped) → a unit vector.
    fn embed(&self, face: &image::RgbImage) -> anyhow::Result<Vec<f32>>;
}

type Plan = Arc<tract_onnx::prelude::TypedRunnableModel>;

/// ONNX models, run on the CPU with tract.
pub struct Onnx {
    detector: Plan,
    det_size: (usize, usize),
    embedder: Plan,
    emb_size: usize,
    name: String,
}

impl Onnx {
    /// `detector`: an UltraFace-style model (input 1×3×H×W, outputs scores
    /// N×2 and boxes N×4, corners 0..1); `embedder`: an ArcFace-style model
    /// (input 1×3×112×112, output an embedding).
    pub fn load(detector: &str, embedder: &str) -> anyhow::Result<Self> {
        use tract_onnx::prelude::*;
        use tract_onnx::tract_hir::infer::Factoid;
        let load = |path: &str, default: (usize, usize)| -> anyhow::Result<(Plan, (usize, usize))> {
            let path = c6_worker::expand_home(path);
            let mut model = tract_onnx::onnx().model_for_path(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            // Its picture size, if it says; else the usual one.
            let fact = model.input_fact(0).map_err(|e| anyhow::anyhow!("{e}"))?.clone();
            let dims: Vec<Option<usize>> =
                fact.shape.dims().map(|d| d.concretize().and_then(|t| t.to_string().parse::<usize>().ok())).collect();
            let (h, w) = match dims.as_slice() {
                [_, _, Some(h), Some(w)] => (*h, *w),
                _ => default,
            };
            model.set_input_fact(0, f32::fact([1, 3, h, w]).into()).map_err(|e| anyhow::anyhow!("{e}"))?;
            let plan = model.into_optimized().and_then(|m| m.into_runnable()).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            Ok((plan, (h, w)))
        };
        let (detector_plan, det_size) = load(detector, (240, 320))?;
        let (embedder_plan, (eh, _)) = load(embedder, (112, 112))?;
        let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
        Ok(Self {
            detector: detector_plan,
            det_size,
            embedder: embedder_plan,
            emb_size: eh,
            name: format!("faces-v1:{}+{}", base(detector), base(embedder)),
        })
    }
}

/// A picture as a 1×3×H×W tensor, (pixel - mean) / scale.
fn tensor(img: &image::RgbImage, h: usize, w: usize, mean: f32, scale: f32) -> tract_onnx::prelude::Tensor {
    let img = image::imageops::resize(img, w as u32, h as u32, image::imageops::FilterType::Triangle);
    tract_onnx::prelude::tract_ndarray::Array4::from_shape_fn((1, 3, h, w), |(_, c, y, x)| {
        (img.get_pixel(x as u32, y as u32)[c] as f32 - mean) / scale
    })
    .into()
}

impl FaceModel for Onnx {
    fn version(&self) -> String {
        self.name.clone()
    }

    fn detect(&self, img: &image::RgbImage) -> anyhow::Result<Vec<Found>> {
        use tract_onnx::prelude::*;
        let (h, w) = self.det_size;
        let out = self.detector.run(tvec!(tensor(img, h, w, 127.0, 128.0).into())).map_err(|e| anyhow::anyhow!("detector: {e}"))?;
        let mut scores = None;
        let mut boxes = None;
        for o in out.iter() {
            let v = o.to_plain_array_view::<f32>().map_err(|e| anyhow::anyhow!("{e}"))?;
            match v.shape().last() {
                Some(2) => scores = Some(v.iter().copied().collect::<Vec<f32>>()),
                Some(4) => boxes = Some(v.iter().copied().collect::<Vec<f32>>()),
                _ => {}
            }
        }
        let (Some(scores), Some(boxes)) = (scores, boxes) else { anyhow::bail!("the detector has no scores and boxes outputs") };
        let mut found: Vec<Found> = (0..scores.len() / 2)
            .filter(|i| scores[i * 2 + 1] > 0.7)
            .filter_map(|i| {
                let b = boxes.get(i * 4..i * 4 + 4)?;
                let (x1, y1, x2, y2) = (b[0].clamp(0.0, 1.0), b[1].clamp(0.0, 1.0), b[2].clamp(0.0, 1.0), b[3].clamp(0.0, 1.0));
                (x2 > x1 && y2 > y1).then_some(Found { x: x1, y: y1, w: x2 - x1, h: y2 - y1, score: scores[i * 2 + 1] })
            })
            .collect();
        Ok(suppress(&mut found))
    }

    fn embed(&self, face: &image::RgbImage) -> anyhow::Result<Vec<f32>> {
        use tract_onnx::prelude::*;
        let s = self.emb_size;
        let out = self.embedder.run(tvec!(tensor(face, s, s, 127.5, 127.5).into())).map_err(|e| anyhow::anyhow!("embedder: {e}"))?;
        let v: Vec<f32> = out[0].to_plain_array_view::<f32>().map_err(|e| anyhow::anyhow!("{e}"))?.iter().copied().collect();
        Ok(unit(v))
    }
}

/// Keeps the best of overlapping boxes.
fn suppress(found: &mut [Found]) -> Vec<Found> {
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Found> = Vec::new();
    for f in found.iter() {
        if kept.iter().all(|k| iou(k, f) < 0.3) {
            kept.push(f.clone());
        }
    }
    kept
}

fn iou(a: &Found, b: &Found) -> f32 {
    let ix = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let iy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if ix <= 0.0 || iy <= 0.0 {
        return 0.0;
    }
    let inter = ix * iy;
    inter / (a.w * a.h + b.w * b.h - inter)
}

pub fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Each person's faces' sum and count.
pub type Index = HashMap<String, (Vec<f32>, u32)>;

async fn index(m: &Memex) -> anyhow::Result<Index> {
    if let Some(i) = m.face_index.lock().unwrap().clone() {
        return Ok(i);
    }
    let mut index: Index = HashMap::new();
    let rows = sqlx::query("SELECT person, vec FROM faces WHERE person IS NOT NULL").fetch_all(&m.store.pool).await?;
    for r in rows {
        let v = from_blob(&r.get::<Vec<u8>, _>("vec"));
        let e = index.entry(r.get("person")).or_insert_with(|| (vec![0.0; v.len()], 0));
        if e.0.len() == v.len() {
            e.0.iter_mut().zip(&v).for_each(|(a, b)| *a += b);
            e.1 += 1;
        }
    }
    *m.face_index.lock().unwrap() = Some(index.clone());
    Ok(index)
}

/// The person a face is most like, if alike enough.
pub fn nearest(index: &Index, v: &[f32]) -> Option<String> {
    index
        .iter()
        .filter(|(_, (sum, _))| sum.len() == v.len())
        .map(|(p, (sum, _))| {
            let n = sum.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
            (p, sum.iter().zip(v).map(|(a, b)| a * b).sum::<f32>() / n)
        })
        .filter(|(_, s)| *s >= THRESHOLD)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p.clone())
}

/// Finds faces in a few more photos. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some(model) = m.face_model.lock().unwrap().clone() else { return Ok(false) };
    let version = model.version();
    let rows = sqlx::query("SELECT item_id, path FROM media WHERE kind = 'photo' AND thumb IS NOT NULL AND (faces_done IS NULL OR faces_done != ?) ORDER BY taken_at DESC LIMIT ?")
        .bind(&version)
        .bind(BATCH)
        .fetch_all(&m.store.pool)
        .await?;
    if rows.is_empty() {
        return Ok(false);
    }
    let mut index = index(m).await?;
    for r in rows {
        let (id, path): (String, String) = (r.get("item_id"), r.get("path"));
        let result = faces_in(m, &model, &id, &path, &mut index).await;
        let error = result.as_ref().err().map(|e| format!("faces: {e:#}"));
        sqlx::query("UPDATE media SET faces_done = ?, error = COALESCE(?, error) WHERE item_id = ?")
            .bind(&version)
            .bind(&error)
            .bind(&id)
            .execute(&m.store.pool)
            .await?;
    }
    *m.face_index.lock().unwrap() = Some(index);
    Ok(true)
}

/// A face found: where, its embedding, and its crop (JPEG).
type Seen = (Found, Vec<f32>, Vec<u8>);

async fn faces_in(m: &Memex, model: &Arc<dyn FaceModel>, id: &str, path: &str, index: &mut Index) -> anyhow::Result<()> {
    let p = std::path::PathBuf::from(path);
    let data = tokio::fs::read(&p).await?;
    let orientation = crate::media::read_exif(&data).orientation;
    let img = crate::media::orient(crate::media::decode(&p, &data).await?, orientation);
    // Big enough to find faces; not the whole 48 megapixels.
    let img = if img.width().max(img.height()) > 1600 { img.thumbnail(1600, 1600) } else { img };
    let model2 = model.clone();
    let found = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Seen>> {
        let rgb = img.to_rgb8();
        let (w, h) = (rgb.width() as f32, rgb.height() as f32);
        let mut out = Vec::new();
        for f in model2.detect(&rgb)? {
            // A square around the face, a little larger.
            let side = (f.w * w).max(f.h * h) * 1.2;
            if side < 24.0 {
                continue;
            }
            let (cx, cy) = ((f.x + f.w / 2.0) * w, (f.y + f.h / 2.0) * h);
            let x0 = (cx - side / 2.0).clamp(0.0, w - 1.0) as u32;
            let y0 = (cy - side / 2.0).clamp(0.0, h - 1.0) as u32;
            let s = (side as u32).min(rgb.width() - x0).min(rgb.height() - y0).max(1);
            let crop = image::imageops::crop_imm(&rgb, x0, y0, s, s).to_image();
            let v = model2.embed(&crop)?;
            let small = image::DynamicImage::ImageRgb8(crop).thumbnail(160, 160);
            out.push((f, v, crate::media::jpeg(&small)?));
        }
        Ok(out)
    })
    .await??;
    // Again from the start (a new model).
    sqlx::query("DELETE FROM faces WHERE item_id = ?").bind(id).execute(&m.store.pool).await?;
    sqlx::query("DELETE FROM links WHERE src_item = ? AND rel = 'depicts' AND made_by LIKE 'faces-%'")
        .bind(id)
        .execute(&m.store.pool)
        .await?;
    for (f, v, crop) in found {
        let person = match nearest(index, &v) {
            Some(p) => p,
            None => {
                let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM things WHERE key LIKE 'face:%'").fetch_one(&m.store.pool).await?;
                m.store.ensure_thing(&format!("face:{}", uuid::Uuid::now_v7()), "person", &format!("Unnamed person {}", n + 1)).await?
            }
        };
        let e = index.entry(person.clone()).or_insert_with(|| (vec![0.0; v.len()], 0));
        e.0.iter_mut().zip(&v).for_each(|(a, b)| *a += b);
        e.1 += 1;
        let hash = m.put_blob(&crop).await?;
        sqlx::query("INSERT INTO faces (item_id, x, y, w, h, score, vec, person, crop) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(id)
            .bind(f.x as f64)
            .bind(f.y as f64)
            .bind(f.w as f64)
            .bind(f.h as f64)
            .bind(f.score as f64)
            .bind(to_blob(&v))
            .bind(&person)
            .bind(&hash)
            .execute(&m.store.pool)
            .await?;
        m.store.link_inferred(id, "depicts", &person, f.score as f64, "faces-v1", None).await?;
        // Someone unnamed, seen a few times: who is it?
        let unnamed: bool = sqlx::query_scalar("SELECT key LIKE 'face:%' FROM things WHERE id = ?")
            .bind(&person)
            .fetch_optional(&m.store.pool)
            .await?
            .unwrap_or(false);
        if unnamed && e.1 as i64 >= ASK_AFTER {
            crate::review::add(&m.store, "face", &person, "", &format!("in {} photos", e.1)).await?;
        }
    }
    Ok(())
}

/// The clearest face of a person, as JPEG.
pub async fn picture(m: &Memex, person: &str) -> anyhow::Result<Vec<u8>> {
    let thing = m.store.thing(person).await?.ok_or_else(|| anyhow::anyhow!("no person {person:?}"))?;
    let hash: String = sqlx::query_scalar("SELECT crop FROM faces WHERE person = ? ORDER BY score * w * h DESC LIMIT 1")
        .bind(&thing.id)
        .fetch_optional(&m.store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no face for {}", thing.name))?;
    Ok(tokio::fs::read(m.blob_path(&hash)).await?)
}

/// Loads the face models from the settings (when they change).
pub async fn configure(m: &Memex, detector: Option<String>, embedder: Option<String>) {
    let key = detector.clone().zip(embedder.clone());
    if *m.face_paths.lock().unwrap() == key {
        return;
    }
    *m.face_paths.lock().unwrap() = key.clone();
    let model: Option<Arc<dyn FaceModel>> = match key {
        Some((d, e)) => match tokio::task::spawn_blocking(move || Onnx::load(&d, &e)).await {
            Ok(Ok(o)) => {
                info!(models = %o.version(), "face models loaded");
                Some(Arc::new(o))
            }
            Ok(Err(e)) => {
                *m.last_error.lock().unwrap() = Some(format!("face models: {e:#}"));
                None
            }
            Err(e) => {
                *m.last_error.lock().unwrap() = Some(format!("face models: {e}"));
                None
            }
        },
        None => None,
    };
    m.set_face_model(model);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_boxes_keep_the_best() {
        let mut f = vec![
            Found { x: 0.1, y: 0.1, w: 0.2, h: 0.2, score: 0.8 },
            Found { x: 0.11, y: 0.1, w: 0.2, h: 0.2, score: 0.95 },
            Found { x: 0.6, y: 0.6, w: 0.2, h: 0.2, score: 0.9 },
        ];
        let kept = suppress(&mut f);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].score, 0.95);
    }

    #[test]
    fn nearest_person() {
        let mut index = Index::new();
        index.insert("a".into(), (vec![2.0, 0.0], 2));
        index.insert("b".into(), (vec![0.0, 1.0], 1));
        assert_eq!(nearest(&index, &[0.9, 0.1]).as_deref(), Some("a"));
        assert_eq!(nearest(&index, &unit(vec![-1.0, -1.0])), None);
    }
}
