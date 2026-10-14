// SPDX-License-Identifier: GPL-3.0-only
use std::fmt::Write;

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");

    // Languages = every lang/<code>/LC_MESSAGES/opendelay.po, named by its
    // "X-Language-Name" header, so adding a language needs no code change.
    let mut langs: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir("lang").expect("lang directory").flatten() {
        let code = entry.file_name().to_string_lossy().into_owned();
        let po = entry.path().join("LC_MESSAGES/opendelay.po");
        let Ok(text) = std::fs::read_to_string(&po) else { continue };
        let name = text
            .lines()
            .find_map(|l| {
                l.trim_matches('"')
                    .strip_prefix("X-Language-Name:")
                    .map(|n| n.trim().trim_end_matches("\\n").to_owned())
            })
            .unwrap_or_else(|| code.clone());
        langs.push((code, name));
    }
    langs.sort();
    let mut out = String::from("pub const LANGUAGES: &[(&str, &str)] = &[(\"en\", \"English\"), ");
    for (code, name) in langs {
        write!(out, "({code:?}, {name:?}), ").unwrap();
    }
    out.push_str("];\n");
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("languages.rs"), out).unwrap();
    println!("cargo:rerun-if-changed=lang");
}
