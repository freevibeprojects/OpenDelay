// SPDX-License-Identifier: GPL-3.0-only
use std::fmt::Write;

fn main() {
    // Languages = every lang/<code>/LC_MESSAGES/opendelay.po, named by its
    // "X-Language-Name" header, so adding a language needs no code change.
    // The UI text itself is English.
    let mut langs: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir("lang").into_iter().flatten().flatten() {
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
    let mut config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    if !langs.is_empty() {
        config = config.with_bundled_translations("lang");
    }
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
    let mut out = String::from("pub const LANGUAGES: &[(&str, &str)] = &[(\"en\", \"English\"), ");
    for (code, name) in langs {
        write!(out, "({code:?}, {name:?}), ").unwrap();
    }
    out.push_str("];\n");
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("languages.rs"), out).unwrap();
    println!("cargo:rerun-if-changed=lang");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_icon();
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            // Load the executable's DLL imports from System32 only, never from
            // its own folder (where a planted DLL could wait, e.g. Downloads).
            println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        }
    }
}

/// Embed `assets/logo.ico` as the executable's icon: write a compiled
/// resource (.res) file by hand and hand it to the linker (link.exe and
/// lld-link both accept .res inputs), so no resource compiler is needed.
/// GNU ld does not: MinGW builds convert it with binutils' `windres`.
fn windows_icon() {
    let ico_path = "../../assets/logo.ico";
    println!("cargo:rerun-if-changed={ico_path}");
    let ico = std::fs::read(ico_path).expect("assets/logo.ico");
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]);
    let count = u16_at(4) as usize;

    // One resource entry: header with numeric type and name, then data, DWORD aligned.
    fn entry(res: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
        res.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
        res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
        res.extend_from_slice(&[0xFF, 0xFF]);
        res.extend_from_slice(&kind.to_le_bytes());
        res.extend_from_slice(&[0xFF, 0xFF]);
        res.extend_from_slice(&id.to_le_bytes());
        res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
        res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
        res.extend_from_slice(&0x0409u16.to_le_bytes()); // LanguageId (en-US)
        res.extend_from_slice(&0u32.to_le_bytes()); // Version
        res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
        res.extend_from_slice(data);
        while !res.len().is_multiple_of(4) {
            res.push(0);
        }
    }

    let mut res = Vec::new();
    entry(&mut res, 0, 0, 0, &[]); // the empty leading entry every .res starts with
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + 16 * i;
        let (size, offset) = (u32_at(e + 8) as usize, u32_at(e + 12) as usize);
        entry(&mut res, 3, (i + 1) as u16, 0x1010, &ico[offset..offset + size]); // RT_ICON
        group.extend_from_slice(&ico[e..e + 12]); // width .. bytes-in-res
        group.extend_from_slice(&((i + 1) as u16).to_le_bytes());
    }
    entry(&mut res, 14, 1, 0x1030, &group); // RT_GROUP_ICON
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("opendelay.res");
    std::fs::write(&out, res).unwrap();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        println!("cargo:rustc-link-arg-bins={}", out.display());
        return;
    }
    let obj = out.with_extension("o");
    let converted = ["x86_64-w64-mingw32-windres", "windres"].iter().any(|tool| {
        std::process::Command::new(tool)
            .args(["--input-format=res", "--output-format=coff", "-i"])
            .arg(&out)
            .arg("-o")
            .arg(&obj)
            .status()
            .is_ok_and(|s| s.success())
    });
    if converted {
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    } else {
        println!("cargo:warning=windres not found: the executable gets no icon");
    }
}
