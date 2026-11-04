//! Where backups go (docs/backups.md §4): a folder (an NFS mount, a disk)
//! or S3-compatible storage. Both store objects by name.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// An object's name and size.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub name: String,
    pub size: u64,
}

#[async_trait]
pub trait Objects: Send + Sync {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()>;
    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// Names under `prefix`, in order.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<Listed>>;
    async fn delete(&self, name: &str) -> anyhow::Result<()>;
}

/// A folder.
pub struct Folder {
    root: PathBuf,
}

impl Folder {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_owned() }
    }

    fn path(&self, name: &str) -> anyhow::Result<PathBuf> {
        if name.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
            bail!("bad object name {name:?}");
        }
        Ok(self.root.join(name))
    }
}

#[async_trait]
impl Objects for Folder {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.path(name)?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use std::io::Write;
            anyhow::ensure!(root.is_dir(), "{} is not there (is it mounted?)", root.display());
            let dir = path.parent().unwrap_or(&root);
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let tmp = path.with_extension(format!("tmp{}", rand::random::<u32>()));
            let mut f = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(&data)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
            Ok(())
        })
        .await?
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.path(name)?;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<Listed>> {
        let root = self.root.clone();
        let prefix = prefix.to_owned();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Listed>> {
            anyhow::ensure!(root.is_dir(), "{} is not there (is it mounted?)", root.display());
            // Walk only the folders the prefix reaches.
            let start = match prefix.rfind('/') {
                Some(i) => root.join(&prefix[..i]),
                None => root.clone(),
            };
            let mut out = Vec::new();
            let mut stack = vec![start];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    let p = e.path();
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() {
                        stack.push(p);
                        continue;
                    }
                    let Ok(rel) = p.strip_prefix(&root) else { continue };
                    let name = rel.to_string_lossy().replace('\\', "/");
                    if name.contains(".tmp") || !name.starts_with(&prefix) {
                        continue;
                    }
                    out.push(Listed { name, size: e.metadata().map(|m| m.len()).unwrap_or(0) });
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await?
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let path = self.path(name)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

/// S3-compatible storage, path-style (`<endpoint>/<bucket>/<prefix><name>`),
/// signed with AWS Signature Version 4.
pub struct S3 {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    pub http: reqwest::Client,
}

/// `s` encoded as SigV4 wants: everything but unreserved characters, and
/// `/` too unless `path`.
fn uri_encode(s: &str, path: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if path => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data.as_bytes()).as_ref().to_vec()
}

/// The `Authorization` header for a request, by SigV4. `headers` are the
/// ones signed (lower-case names), `host` among them.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    method: &str,
    uri: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_sha256: &str,
    amz_date: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_owned())).collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{uri}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_sha256}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{secret_key}").as_bytes(), date);
    let k = hmac(&k, region);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}")
}

fn amz_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// The text of each `<tag>…</tag>` in `xml`, unescaped.
fn tags<'a>(xml: &'a str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest: &'a str = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"));
        rest = &after[j + close.len()..];
    }
    out
}

impl S3 {
    async fn send(
        &self,
        method: reqwest::Method,
        key: Option<&str>,
        query: &[(String, String)],
        body: Vec<u8>,
    ) -> anyhow::Result<reqwest::Response> {
        let base = url::Url::parse(self.endpoint.trim_end_matches('/')).context("the S3 endpoint is not a URL")?;
        let host = match base.port() {
            Some(p) => format!("{}:{p}", base.host_str().unwrap_or_default()),
            None => base.host_str().unwrap_or_default().to_owned(),
        };
        let base_path = base.path().trim_end_matches('/');
        let mut uri = format!("{base_path}/{}", uri_encode(&self.bucket, false));
        if let Some(k) = key {
            uri.push('/');
            uri.push_str(&uri_encode(&format!("{}{k}", self.prefix), true));
        }
        let payload = hex::encode(Sha256::digest(&body));
        let date = amz_now();
        let headers = vec![
            ("host".to_owned(), host.clone()),
            ("x-amz-content-sha256".to_owned(), payload.clone()),
            ("x-amz-date".to_owned(), date.clone()),
        ];
        let auth = authorization(method.as_str(), &uri, query, &headers, &payload, &date, &self.region, &self.access_key, &self.secret_key);
        let mut url = format!("{}://{host}{uri}", base.scheme());
        if !query.is_empty() {
            let q: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect();
            url = format!("{url}?{}", q.join("&"));
        }
        let r = self
            .http
            .request(method, &url)
            .header("x-amz-content-sha256", payload)
            .header("x-amz-date", date)
            .header("authorization", auth)
            .body(body)
            .send()
            .await
            .with_context(|| format!("reaching {}", self.endpoint))?;
        Ok(r)
    }

    async fn ok(r: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
        if r.status().is_success() {
            return Ok(r);
        }
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        let msg = tags(&text, "Message").into_iter().next().unwrap_or(text);
        bail!("S3 {what}: {status} {}", crate::util_clip(&msg, 300))
    }
}

#[async_trait]
impl Objects for S3 {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let r = self.send(reqwest::Method::PUT, Some(name), &[], data).await?;
        Self::ok(r, &format!("writing {name}")).await?;
        Ok(())
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let r = self.send(reqwest::Method::GET, Some(name), &[], Vec::new()).await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::ok(r, &format!("reading {name}")).await?.bytes().await?.to_vec()))
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<Listed>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type".to_owned(), "2".to_owned()), ("prefix".to_owned(), format!("{}{prefix}", self.prefix))];
            if let Some(t) = &token {
                q.push(("continuation-token".to_owned(), t.clone()));
            }
            let r = self.send(reqwest::Method::GET, None, &q, Vec::new()).await?;
            let text = Self::ok(r, "listing").await?.text().await?;
            for c in tags(&text, "Contents") {
                let Some(key) = tags(&c, "Key").into_iter().next() else { continue };
                let size = tags(&c, "Size").into_iter().next().and_then(|s| s.parse().ok()).unwrap_or(0);
                if let Some(name) = key.strip_prefix(&self.prefix) {
                    out.push(Listed { name: name.to_owned(), size });
                }
            }
            token = tags(&text, "NextContinuationToken").into_iter().next();
            if tags(&text, "IsTruncated").first().map(String::as_str) != Some("true") || token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let r = self.send(reqwest::Method::DELETE, Some(name), &[], Vec::new()).await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::ok(r, &format!("removing {name}")).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own example (S3 documentation, "Example: GET Object").
    #[test]
    fn signs_as_aws_does() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let auth = authorization(
            "GET",
            "/test.txt",
            &[],
            &[
                ("host".into(), "examplebucket.s3.amazonaws.com".into()),
                ("range".into(), "bytes=0-9".into()),
                ("x-amz-content-sha256".into(), empty.into()),
                ("x-amz-date".into(), "20130524T000000Z".into()),
            ],
            empty,
            "20130524T000000Z",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    async fn round_trip(o: &dyn Objects) {
        o.put("db/a/1", b"one".to_vec()).await.unwrap();
        o.put("db/a/2", b"two".to_vec()).await.unwrap();
        o.put("db/b/1", b"three".to_vec()).await.unwrap();
        o.put("keys/names", b"k".to_vec()).await.unwrap();
        assert_eq!(o.get("db/a/2").await.unwrap().as_deref(), Some(b"two".as_slice()));
        assert_eq!(o.get("db/a/9").await.unwrap(), None);
        let names: Vec<String> = o.list("db/a/").await.unwrap().into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["db/a/1", "db/a/2"]);
        assert_eq!(o.list("db/").await.unwrap().len(), 3);
        assert_eq!(o.list("").await.unwrap().len(), 4);
        o.delete("db/a/1").await.unwrap();
        o.delete("db/a/1").await.unwrap();
        assert_eq!(o.list("db/a/").await.unwrap(), vec![Listed { name: "db/a/2".into(), size: 3 }]);
    }

    #[tokio::test]
    async fn a_folder_stores_objects() {
        let dir = tempfile::tempdir().unwrap();
        round_trip(&Folder::new(dir.path())).await;
        assert!(Folder::new(&dir.path().join("gone")).put("x", vec![]).await.is_err(), "an unmounted folder says so");
        assert!(Folder::new(dir.path()).put("../x", vec![]).await.is_err());
    }

    #[tokio::test]
    async fn s3_stores_objects() {
        c6_llm::install_crypto_provider();
        let s3 = c6_testkit::MockS3::start("AKTEST", "bucket").await;
        let o = S3 {
            endpoint: s3.endpoint(),
            bucket: "bucket".into(),
            region: "us-east-1".into(),
            prefix: "c6/".into(),
            access_key: "AKTEST".into(),
            secret_key: "secret".into(),
            http: reqwest::Client::new(),
        };
        round_trip(&o).await;
        assert!(s3.keys().iter().all(|k| k.starts_with("c6/")), "{:?}", s3.keys());
        // More than a page of names.
        for i in 0..25 {
            o.put(&format!("many/{i:03}"), vec![]).await.unwrap();
        }
        assert_eq!(o.list("many/").await.unwrap().len(), 25);
        let wrong = S3 { access_key: "OTHER".into(), ..o };
        assert!(wrong.put("x", vec![]).await.is_err(), "a wrong key is refused");
    }
}
