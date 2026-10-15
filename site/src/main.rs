//! Builds the carbon6.dev website into a directory of static files.
//!
//! Every page is a Leptos view rendered to HTML on the build machine; the site
//! ships no wasm and no JavaScript. The design docs and the changelog come
//! from the repository's own markdown, so they stay in step with the code.
//!
//!     c6-site [--out <dir>] [--root <repo>] [--base-url <url>]

mod docs;
mod markdown;
mod pages;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use leptos::prelude::*;

/// What every page needs to know about the build.
#[derive(Clone)]
pub struct Site {
    /// The repository checkout the content comes from.
    pub root: PathBuf,
    /// The workspace version: the latest release, except between a release
    /// commit and its published assets.
    pub version: String,
    /// Where the site is served, without a trailing slash.
    pub base_url: String,
    pub repo_url: String,
    /// Changes with the stylesheet, so browsers fetch a new one.
    pub asset_version: String,
}

/// One output file: a site path ("/install/") and its HTML.
pub struct Page {
    pub path: String,
    pub html: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("c6-site: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut root = manifest_dir.parent().unwrap().to_path_buf();
    let mut out = root.join("dist/site");
    let mut base_url = "https://carbon6.dev".to_string();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => out = PathBuf::from(value()?),
            "--root" => root = PathBuf::from(value()?),
            "--base-url" => base_url = value()?.trim_end_matches('/').to_string(),
            "-h" | "--help" => {
                println!("usage: c6-site [--out <dir>] [--root <repo>] [--base-url <url>]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }

    let cargo_toml = read(&root.join("Cargo.toml"))?;
    let version = cargo_toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"'))
        .ok_or("no workspace version in Cargo.toml")?
        .to_string();
    let static_dir = manifest_dir.join("static");
    let asset_version = format!("{:08x}", fnv1a(read(&static_dir.join("style.css"))?.as_bytes()) as u32);
    let site = Site { root, version, base_url, repo_url: "https://github.com/carbon6-dev/carbon6".into(), asset_version };

    let pages = pages::all(&site)?;
    let static_files: Vec<String> = fs::read_dir(&static_dir)
        .map_err(|e| format!("{}: {e}", static_dir.display()))?
        .filter_map(|e| Some(format!("/{}", e.ok()?.file_name().to_str()?)))
        .collect();
    let broken = broken_links(&pages, &static_files);
    if !broken.is_empty() {
        return Err(format!("broken links:\n  {}", broken.join("\n  ")));
    }

    // Start from an empty directory, so a page that is gone from the site is
    // gone from the output too.
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    for page in &pages {
        let file = if page.path.ends_with('/') {
            out.join(page.path.trim_start_matches('/')).join("index.html")
        } else {
            out.join(page.path.trim_start_matches('/'))
        };
        write(&file, page.html.as_bytes())?;
    }
    for entry in fs::read_dir(&static_dir).map_err(|e| format!("{}: {e}", static_dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let bytes = fs::read(entry.path()).map_err(|e| format!("{}: {e}", entry.path().display()))?;
        write(&out.join(entry.file_name()), &bytes)?;
    }
    // GitHub Pages: serve the files as they are (no Jekyll), on our domain.
    write(&out.join(".nojekyll"), b"")?;
    let host = site.base_url.split("://").nth(1).unwrap_or(&site.base_url);
    write(&out.join("CNAME"), format!("{host}\n").as_bytes())?;
    write(&out.join("sitemap.xml"), sitemap(&site, &pages).as_bytes())?;
    write(&out.join("robots.txt"), format!("User-agent: *\nAllow: /\nSitemap: {}/sitemap.xml\n", site.base_url).as_bytes())?;

    println!("c6-site: {} pages for {} (carbon6 {}) in {}", pages.len(), site.base_url, site.version, out.display());
    Ok(())
}

/// Renders a page's view to a complete HTML document.
pub fn render<V: IntoView>(site: &Site, view: impl FnOnce() -> V) -> String {
    let owner = Owner::new();
    owner.with(|| {
        provide_context(site.clone());
        // `<!>` marks text boundaries for hydration, which a static page
        // doesn't do. Text never contains it: its `<` is escaped.
        format!("<!DOCTYPE html>\n{}", view().into_view().to_html().replace("<!>", ""))
    })
}

/// Links within the site that go nowhere: to a page or file the site doesn't
/// have, or to an id the page doesn't have. ("page → link")
fn broken_links(pages: &[Page], static_files: &[String]) -> Vec<String> {
    let find = |path: &str| pages.iter().find(|p| p.path == path);
    let mut broken = Vec::new();
    for page in pages {
        for href in page.html.split("href=\"").skip(1).filter_map(|s| s.split('"').next()) {
            if href.contains("://") || href.starts_with("mailto:") {
                continue;
            }
            let (path, fragment) = href.split_once('#').unwrap_or((href, ""));
            let path = path.split('?').next().unwrap_or_default();
            let target = if path.is_empty() { Some(page) } else { find(path) };
            let ok = match target {
                Some(target) => fragment.is_empty() || target.html.contains(&format!(" id=\"{fragment}\"")),
                None => fragment.is_empty() && static_files.iter().any(|f| f == path),
            };
            if !ok {
                broken.push(format!("{} → {href}", page.path));
            }
        }
    }
    broken
}

fn sitemap(site: &Site, pages: &[Page]) -> String {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for page in pages.iter().filter(|p| p.path.ends_with('/')) {
        xml.push_str(&format!("  <url><loc>{}{}</loc></url>\n", site.base_url, page.path));
    }
    xml.push_str("</urlset>\n");
    xml
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

pub fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_to_missing_pages_files_and_ids_are_broken() {
        let page = |path: &str, html: &str| Page { path: path.into(), html: html.into() };
        let pages = [
            page(
                "/",
                r##"<p id="top"></p><a href="/docs/">x</a><a href="/style.css?v=1">x</a><a href="https://x.dev/no">x</a><a href="#top">x</a>"##,
            ),
            page(
                "/docs/",
                r##"<h2 id="a">A</h2><a href="/#top">x</a><a href="/docs/#a">x</a><a href="/no/">x</a><a href="/docs/#b">x</a>"##,
            ),
        ];
        assert_eq!(broken_links(&pages, &["/style.css".into()]), ["/docs/ → /no/", "/docs/ → /docs/#b"]);
    }
}
