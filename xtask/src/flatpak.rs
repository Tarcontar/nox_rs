//! `cargo xtask flatpak-sources`: the crates in `Cargo.lock` as Flatpak
//! sources (`packaging/flatpak/cargo-sources.json`), since Flatpak builds
//! have no network. Each crate is downloaded from crates.io into
//! `cargo/vendor`, which a generated `cargo/config.toml` makes Cargo's
//! only source; Cargo checks each crate against the lock file's checksum.
//! Does what Flathub's flatpak-cargo-generator does for crates.io
//! dependencies, without Python.

use serde_json::{Value, json};

use crate::{Result, root};

const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

pub fn sources() -> Result {
    let lock = std::fs::read_to_string(root().join("Cargo.lock")).map_err(|e| e.to_string())?;
    let mut sources = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            block.lines().find_map(|line| {
                let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?;
                Some(value.trim().trim_matches('"').to_owned())
            })
        };
        let (Some(name), Some(version)) = (field("name"), field("version")) else {
            continue;
        };
        match field("source") {
            None => continue, // the workspace's own crates
            Some(source) if source == REGISTRY => {}
            Some(other) => return Err(format!("{name}: unsupported source {other}")),
        }
        let checksum = field("checksum").ok_or(format!("{name}: no checksum"))?;
        let dest = format!("cargo/vendor/{name}-{version}");
        sources.push(json!({
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": format!("https://static.crates.io/crates/{name}/{name}-{version}.crate"),
            "sha256": checksum,
            "dest": dest,
        }));
        sources.push(json!({
            "type": "inline",
            "contents": json!({ "package": checksum, "files": {} }).to_string(),
            "dest": dest,
            "dest-filename": ".cargo-checksum.json",
        }));
    }
    let count = sources.len() / 2;
    sources.push(json!({
        "type": "inline",
        "contents": "[source.vendored-sources]\ndirectory = \"cargo/vendor\"\n\n\
                     [source.crates-io]\nreplace-with = \"vendored-sources\"\n",
        "dest": "cargo",
        "dest-filename": "config.toml",
    }));
    let path = root().join("packaging/flatpak/cargo-sources.json");
    std::fs::create_dir_all(path.parent().expect("dir")).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&Value::Array(sources)).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| e.to_string())?;
    println!("{} ({count} crates)", path.display());
    Ok(())
}
