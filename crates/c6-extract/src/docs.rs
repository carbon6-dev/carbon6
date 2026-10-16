//! Documents and papers: text from PDF, Word, HTML, Markdown and text
//! files; whether a document is a research paper, and if so its title,
//! authors, identifiers (DOI, arXiv) and the papers it cites.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

static DOI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(10\.\d{4,9}/[-._;()/:A-Za-z0-9]+[A-Za-z0-9])").unwrap());
static ARXIV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:arxiv:\s*|arxiv\.org/(?:abs|pdf)/)(\d{4}\.\d{4,5})").unwrap());
static REF_NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*(?:\[\d{1,3}\]|\d{1,3}\.)\s+").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[“"]([^”"]{15,300})[”"]"#).unwrap());
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}[a-z]?\b").unwrap());

/// The text of a file, by its name's extension (and for PDFs, its
/// metadata title, if any).
pub fn extract(name: &str, data: &[u8]) -> anyhow::Result<(String, Option<String>)> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    Ok(match ext.as_str() {
        "pdf" => {
            let text = pdf_extract::extract_text_from_mem(data).map_err(|e| anyhow::anyhow!("reading the PDF: {e}"))?;
            (clean(&text), pdf_title(data))
        }
        "docx" => (docx_text(data)?, None),
        "html" | "htm" => {
            let html = String::from_utf8_lossy(data);
            (htmd::convert(&html).unwrap_or_else(|_| html.into_owned()), None)
        }
        _ => (String::from_utf8_lossy(data).into_owned(), None),
    })
}

/// Collapses runs of spaces and blank lines that PDF extraction leaves.
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")) {
        if line.is_empty() {
            blank += 1;
            if blank == 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

/// The /Title in a PDF's document information, if it looks like a title.
fn pdf_title(data: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(&data[..data.len().min(2_000_000)]);
    let i = s.find("/Title")?;
    let rest = s[i + 6..].trim_start();
    let title = rest.strip_prefix('(')?.split(')').next()?.trim().to_owned();
    let ok = title.len() >= 8
        && title.len() <= 300
        && !title.to_lowercase().contains("untitled")
        && !title.contains(".dvi")
        && !title.contains(".tex");
    ok.then_some(title)
}

/// The text of a .docx: its paragraphs' runs.
fn docx_text(data: &[u8]) -> anyhow::Result<String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")?.read_to_string(&mut xml)?;
    let mut out = String::new();
    for para in xml.split("</w:p>") {
        let mut line = String::new();
        let mut rest = para;
        while let Some(i) = rest.find("<w:t") {
            let after = &rest[i..];
            let Some(open_end) = after.find('>') else { break };
            if after[..open_end].ends_with('/') {
                rest = &after[open_end + 1..];
                continue;
            }
            let body = &after[open_end + 1..];
            let Some(close) = body.find("</w:t>") else { break };
            line.push_str(&unescape(&body[..close]));
            rest = &body[close..];
        }
        if !line.trim().is_empty() {
            out.push_str(line.trim());
            out.push_str("\n\n");
        }
    }
    Ok(out.trim().to_owned())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Words that mark an affiliation, not a person.
const ORG_WORDS: [&str; 24] = [
    "University",
    "Institute",
    "Department",
    "Laboratory",
    "Laboratories",
    "Lab",
    "Labs",
    "Inc",
    "Inc.",
    "Corp",
    "Corporation",
    "Research",
    "Google",
    "Brain",
    "Microsoft",
    "Meta",
    "DeepMind",
    "OpenAI",
    "Anthropic",
    "Center",
    "Centre",
    "School",
    "College",
    "Abstract",
];

/// A research paper, as read from its text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Paper {
    pub title: String,
    pub authors: Vec<String>,
    pub doi: Option<String>,
    pub arxiv: Option<String>,
    pub year: Option<String>,
    pub references: Vec<Reference>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reference {
    /// As written.
    pub text: String,
    pub title: String,
    pub doi: Option<String>,
    pub arxiv: Option<String>,
}

impl Reference {
    /// How cited papers are told apart: DOI, else arXiv id, else title.
    pub fn key(&self) -> String {
        paper_key(self.doi.as_deref(), self.arxiv.as_deref(), &self.title)
    }
}

pub fn paper_key(doi: Option<&str>, arxiv: Option<&str>, title: &str) -> String {
    if let Some(d) = doi {
        return format!("paper:doi:{}", d.to_lowercase());
    }
    if let Some(a) = arxiv {
        return format!("paper:arxiv:{a}");
    }
    let norm: String = title.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
    let norm = norm.split_whitespace().collect::<Vec<_>>().join("-");
    format!("paper:title:{}", norm.chars().take(80).collect::<String>())
}

/// The paper this text is, if it reads like one: an abstract and a
/// reference list, or a DOI or arXiv id up front.
pub fn analyze(text: &str, meta_title: Option<&str>) -> Option<Paper> {
    let lower = text.to_lowercase();
    let refs_at = ["\nreferences\n", "\nbibliography\n", "\nreferences \n", "\nreferences:\n", "\nworks cited\n"]
        .iter()
        .filter_map(|h| lower.rfind(h))
        .max();
    // The paper's own identifiers come before its reference list.
    let body = &text[..refs_at.unwrap_or(text.len())];
    let head: String = body.chars().take(5000).collect();
    let has_abstract = head.to_lowercase().contains("abstract");
    let doi = DOI.captures(&head).map(|c| c[1].trim_end_matches('.').to_owned());
    let arxiv = ARXIV.captures(&head).map(|c| c[1].to_owned());
    if !((has_abstract && refs_at.is_some()) || doi.is_some() || arxiv.is_some()) {
        return None;
    }
    // Title: the metadata's, else the first line of the first page that
    // reads like one.
    let lines: Vec<&str> = head.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let title_at = lines.iter().position(|l| {
        let words = l.split_whitespace().count();
        (3..=30).contains(&words) && !l.to_lowercase().starts_with("arxiv") && !DOI.is_match(l) && !l.contains('@') && l.len() <= 250
    });
    let title = meta_title.map(str::to_owned).or_else(|| title_at.map(|i| lines[i].to_owned())).unwrap_or_default();
    // Authors: between the title and the abstract, names separated by
    // commas or "and".
    let mut authors = Vec::new();
    if let Some(t) = title_at {
        for l in lines.iter().skip(t + 1).take(8) {
            if l.to_lowercase().starts_with("abstract") {
                break;
            }
            for part in l.split([',', ';']).flat_map(|p| p.split(" and ")) {
                let name = part.trim().trim_end_matches(|c: char| c.is_ascii_digit() || "*†‡§".contains(c)).trim();
                let words: Vec<&str> = name.split_whitespace().collect();
                let looks = (2..=4).contains(&words.len())
                    && words.iter().all(|w| w.chars().next().is_some_and(char::is_uppercase))
                    && !name.chars().any(|c| c.is_ascii_digit() || c == '@')
                    && !ORG_WORDS.iter().any(|x| words.contains(x));
                if looks && !authors.contains(&name.to_owned()) && authors.len() < 20 {
                    authors.push(name.to_owned());
                }
            }
        }
    }
    let year = YEAR.find(&head).map(|m| m.as_str()[..4].to_owned());
    let references = refs_at.map(|at| references(&text[at..])).unwrap_or_default();
    Some(Paper { title, authors, doi, arxiv, year, references })
}

/// The entries of a reference list.
fn references(section: &str) -> Vec<Reference> {
    let body = section.split_once('\n').map(|(_, b)| b).unwrap_or("");
    let body = body.split_once('\n').map(|(first, rest)| if first.trim().is_empty() { rest } else { body }).unwrap_or(body);
    let entries: Vec<String> = if REF_NUMBERED.find_iter(body).count() >= 2 {
        REF_NUMBERED.split(body).map(|e| e.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|e| e.len() > 20).collect()
    } else {
        body.split("\n\n").map(|e| e.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|e| e.len() > 20).collect()
    };
    entries
        .into_iter()
        .take(400)
        .map(|e| {
            let doi = DOI.captures(&e).map(|c| c[1].trim_end_matches('.').to_owned());
            let arxiv = ARXIV.captures(&e).map(|c| c[1].to_owned());
            let title = QUOTED.captures(&e).map(|c| c[1].trim().trim_end_matches(',').to_owned()).unwrap_or_else(|| guess_title(&e));
            Reference { text: e.chars().take(600).collect(), title, doi, arxiv }
        })
        .collect()
}

/// In "A. Author, B. Author. The title of it. Venue, 2020.", the title:
/// the first sentence after the year or the authors that is long enough.
fn guess_title(entry: &str) -> String {
    let after_year = YEAR.find(entry).filter(|m| m.start() < entry.len() / 2).map(|m| &entry[m.end()..]).unwrap_or(entry);
    let sentences: Vec<&str> =
        after_year.split(". ").map(|s| s.trim_matches(|c: char| c == ')' || c == '(' || c == '.' || c == ' ')).collect();
    sentences
        .iter()
        .find(|s| {
            let lower = s.to_lowercase();
            s.len() >= 8
                && s.split_whitespace().count() >= 2
                && !s.contains("et al")
                && !names_list(s)
                && !["arxiv", "doi", "in ", "proc", "http", "vol", "pp"].iter().any(|p| lower.starts_with(p))
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| entry.chars().take(120).collect())
}

/// "A. Author, B. Author and C. Author": mostly capitalized words joined
/// by commas or "and".
fn names_list(s: &str) -> bool {
    let words: Vec<&str> = s.split_whitespace().collect();
    let capital = words.iter().filter(|w| w.chars().next().is_some_and(char::is_uppercase) || **w == "and").count();
    (s.contains(',') || s.contains(" and ")) && capital * 10 >= words.len() * 8
}

/// What the knowledge store keeps of a document: its text, its metadata
/// title, and if it is a paper, what it says about itself and cites.
pub fn document(name: &str, data: &[u8]) -> anyhow::Result<Value> {
    let (text, meta_title) = extract(name, data)?;
    let paper = if name.to_lowercase().ends_with(".pdf") { analyze(&text, meta_title.as_deref()) } else { None };
    Ok(json!({
        "text": text,
        "title": meta_title,
        "paper": paper.map(|p| json!({
            "key": paper_key(p.doi.as_deref(), p.arxiv.as_deref(), &p.title),
            "title": p.title, "authors": p.authors, "doi": p.doi, "arxiv": p.arxiv, "year": p.year,
            "references": p.references.iter().map(|r| json!({"key": r.key(), "title": r.title, "text": r.text})).collect::<Vec<_>>(),
        })),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER: &str = "Attention Is All You Need
Ashish Vaswani, Noam Shazeer and Niki Parmar
Google Brain
arXiv:1706.03762v5
Abstract
The dominant sequence transduction models are based on complex recurrent networks.

1 Introduction
Recurrent neural networks have been established as state of the art.

References
[1] Jimmy Lei Ba, Jamie Ryan Kiros, and Geoffrey E Hinton. Layer normalization. arXiv preprint arXiv:1607.06450, 2016.
[2] Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua Bengio. Neural machine translation by jointly learning to align and translate. CoRR, abs/1409.0473, 2014.
[3] K. He, X. Zhang. \"Deep residual learning for image recognition\". In CVPR, 2016. doi:10.1109/CVPR.2016.90
";

    #[test]
    fn papers() {
        let p = analyze(PAPER, None).expect("a paper");
        assert_eq!(p.title, "Attention Is All You Need");
        assert_eq!(p.authors, ["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"]);
        assert_eq!(p.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(p.references.len(), 3, "{:?}", p.references);
        assert_eq!(p.references[0].arxiv.as_deref(), Some("1607.06450"));
        assert_eq!(p.references[0].key(), "paper:arxiv:1607.06450");
        assert!(p.references[1].title.starts_with("Neural machine translation by jointly learning"), "{:?}", p.references[1]);
        assert_eq!(p.references[2].title, "Deep residual learning for image recognition");
        assert_eq!(p.references[2].key(), "paper:doi:10.1109/cvpr.2016.90");
        assert!(analyze("Meeting notes\n\nWe talked about the budget.", None).is_none());
    }

    #[test]
    fn docx() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(br#"<w:document><w:body><w:p><w:r><w:t>Quarterly plan</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Ship the </w:t></w:r><w:r><w:t>memex &amp; mail</w:t></w:r></w:p></w:body></w:document>"#).unwrap();
            z.finish().unwrap();
        }
        let (text, _) = extract("plan.docx", buf.get_ref()).unwrap();
        assert_eq!(text, "Quarterly plan\n\nShip the memex & mail");
    }
}
