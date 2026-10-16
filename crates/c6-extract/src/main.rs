//! `c6-extract <jobs.json>`: runs the jobs, writing `out/result.json` (and
//! each job's files) beside it. See the library.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: c6-extract <jobs.json>");
        std::process::exit(2);
    };
    let path = std::path::PathBuf::from(path);
    let req: serde_json::Value =
        match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                std::process::exit(2);
            }
        };
    let out = path.parent().unwrap_or(std::path::Path::new(".")).join("out");
    let result = c6_extract::run(&req, &out, None);
    if let Err(e) = std::fs::create_dir_all(&out).and_then(|_| std::fs::write(out.join("result.json"), result.to_string())) {
        eprintln!("writing the result: {e}");
        std::process::exit(1);
    }
}
