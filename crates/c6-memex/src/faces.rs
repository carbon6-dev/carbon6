//! Faces in photos: found and described by the c6-extract kit with the
//! ONNX models you give (e.g. UltraFace and ArcFace or MobileFaceNet), and
//! grouped here: a face like the faces of someone (closer than THRESHOLD
//! to their average) is theirs; otherwise it starts a new, unnamed person.
//! Once someone has a few photos, naming them is in the review queue, and
//! naming joins them to the person you mean (the one you email, say).

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;

/// How alike two faces' embeddings must be (cosine) to be one person.
const THRESHOLD: f32 = 0.45;
/// Faces before an unnamed person is put to you.
const ASK_AFTER: i64 = 3;
/// Photos asked for at a time.
const BATCH: i64 = 24;

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

/// The face models' name (stored with what they found): their files'.
fn version(m: &Memex) -> Option<String> {
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
    m.face_paths.lock().unwrap().as_ref().map(|(d, e)| format!("faces-v1:{}+{}", base(d), base(e)))
}

/// Asks the kit for the faces in a few more photos. Whether it did.
pub async fn step(m: &Memex) -> anyhow::Result<bool> {
    let Some(version) = version(m) else { return Ok(false) };
    let rows = sqlx::query(
        "SELECT md.item_id, md.path, i.title FROM media md JOIN items i ON i.id = md.item_id WHERE md.kind = 'photo' \
         AND md.thumb IS NOT NULL AND (md.faces_done IS NULL OR md.faces_done != ?) \
         AND ('faces:' || md.item_id) NOT IN (SELECT key FROM extractions) ORDER BY md.taken_at DESC LIMIT ?",
    )
    .bind(&version)
    .bind(BATCH)
    .fetch_all(&m.store.pool)
    .await?;
    for r in &rows {
        let id: String = r.get("item_id");
        let params = json!({"item": id, "version": version});
        crate::extract::queue(m, &format!("faces:{id}"), "faces", &r.get::<String, _>("path"), &r.get::<String, _>("title"), params)
            .await?;
    }
    Ok(!rows.is_empty())
}

/// Keeps the faces the kit found in a photo: each one's person (the one
/// it is most like, or someone new), and asks who someone unnamed is.
pub async fn from(m: &Memex, job: &crate::extract::Done<'_>, v: &Value) -> anyhow::Result<String> {
    let id = job.params["item"].as_str().unwrap_or_default().to_owned();
    let version = job.params["version"].as_str().unwrap_or_default();
    let mut index = index(m).await?;
    // Again from the start (a new model).
    sqlx::query("DELETE FROM faces WHERE item_id = ?").bind(&id).execute(&m.store.pool).await?;
    sqlx::query("DELETE FROM links WHERE src_item = ? AND rel = 'depicts' AND made_by LIKE 'faces-%'")
        .bind(&id)
        .execute(&m.store.pool)
        .await?;
    for (i, f) in v["faces"].as_array().into_iter().flatten().enumerate() {
        let vec: Vec<f32> = serde_json::from_value(f["vec"].clone()).unwrap_or_default();
        let Some(crop) = job.files.get(&format!("face-{i}.jpg")) else { continue };
        if vec.is_empty() {
            continue;
        }
        let g = |k: &str| f[k].as_f64().unwrap_or(0.0);
        let person = match nearest(&index, &vec) {
            Some(p) => p,
            None => {
                let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM things WHERE key LIKE 'face:%'").fetch_one(&m.store.pool).await?;
                m.store.ensure_thing(&format!("face:{}", uuid::Uuid::now_v7()), "person", &format!("Unnamed person {}", n + 1)).await?
            }
        };
        let e = index.entry(person.clone()).or_insert_with(|| (vec![0.0; vec.len()], 0));
        e.0.iter_mut().zip(&vec).for_each(|(a, b)| *a += b);
        e.1 += 1;
        let seen = e.1;
        let hash = m.put_blob(crop).await?;
        sqlx::query("INSERT INTO faces (item_id, x, y, w, h, score, vec, person, crop) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(g("x"))
            .bind(g("y"))
            .bind(g("w"))
            .bind(g("h"))
            .bind(g("score"))
            .bind(to_blob(&vec))
            .bind(&person)
            .bind(&hash)
            .execute(&m.store.pool)
            .await?;
        m.store.link_inferred(&id, "depicts", &person, g("score"), "faces-v1", None).await?;
        // Someone unnamed, seen a few times: who is it?
        let unnamed: bool = sqlx::query_scalar("SELECT key LIKE 'face:%' FROM things WHERE id = ?")
            .bind(&person)
            .fetch_optional(&m.store.pool)
            .await?
            .unwrap_or(false);
        if unnamed && seen as i64 >= ASK_AFTER {
            crate::review::add(&m.store, "face", &person, "", &format!("in {seen} photos")).await?;
        }
    }
    *m.face_index.lock().unwrap() = Some(index);
    sqlx::query("UPDATE media SET faces_done = ? WHERE item_id = ?").bind(version).bind(&id).execute(&m.store.pool).await?;
    Ok(id)
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

/// Uses the face models in the settings (when they change): the files go
/// to the kit with each batch.
pub async fn configure(m: &Memex, detector: Option<String>, embedder: Option<String>) {
    let key = detector.zip(embedder);
    if *m.face_paths.lock().unwrap() == key {
        return;
    }
    *m.face_paths.lock().unwrap() = key;
    *m.face_index.lock().unwrap() = None;
    m.wake.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_person() {
        let mut index = Index::new();
        index.insert("a".into(), (vec![2.0, 0.0], 2));
        index.insert("b".into(), (vec![0.0, 1.0], 1));
        assert_eq!(nearest(&index, &[0.9, 0.1]).as_deref(), Some("a"));
        assert_eq!(nearest(&index, &unit(vec![-1.0, -1.0])), None);
    }
}
