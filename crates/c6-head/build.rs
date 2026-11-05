fn main() {
    // The web UI is embedded with rust-embed; rebuild when `make ui` changes it.
    // A missing watched path is always dirty in Cargo. Until dist exists,
    // watch its existing parent so creating it still triggers a rebuild.
    let ui = std::path::Path::new("../c6-ui");
    let dist = ui.join("dist");
    let watch = if dist.is_dir() { &dist } else { ui };
    println!("cargo:rerun-if-changed={}", watch.display());
}
