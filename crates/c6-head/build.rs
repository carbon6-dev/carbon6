fn main() {
    // The web UI is embedded with rust-embed; rebuild when `make ui` changes it.
    println!("cargo:rerun-if-changed=../c6-ui/dist");
}
