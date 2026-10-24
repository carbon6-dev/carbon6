//! `c6-slack <tool>`: runs a tool in /work (see the library).

fn main() {
    let Some(tool) = std::env::args().nth(1) else {
        eprintln!("usage: c6-slack <tool>");
        std::process::exit(2);
    };
    let work = std::env::var("C6_WORK").unwrap_or_else(|_| "/work".into());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    if let Err(e) = rt.block_on(c6_slack::run(&tool, std::path::Path::new(&work), &c6_slack::Env::from_env())) {
        eprintln!("{tool}: {e:#}");
        std::process::exit(1);
    }
}
