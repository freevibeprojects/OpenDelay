// SPDX-License-Identifier: GPL-3.0-only
//! Embeds every web translation file (`web/lang/<code>.json`), so adding a
//! language for the dock/overlay pages needs no code change.

fn main() {
    let dir = std::path::Path::new("web/lang");
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .expect("web/lang")
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_owned))
        .collect();
    files.sort();
    let mut out = String::from("pub const WEB_LANGS: &[(&str, &[u8])] = &[\n");
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    for code in files {
        out.push_str(&format!("    ({code:?}, include_bytes!({:?})),\n", format!("{root}/web/lang/{code}.json")));
    }
    out.push_str("];\n");
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("web_langs.rs"), out).unwrap();
    println!("cargo:rerun-if-changed=web/lang");
}
