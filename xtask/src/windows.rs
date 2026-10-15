//! The self-contained Windows package, built in MSYS2 (CLANG64 or
//! CLANGARM64, whose GTK, librsvg and codec libraries Nox links; FFmpeg
//! from `packaging/ffmpeg/build.sh` installed into the same prefix):
//!
//! ```text
//! nox-<version>-windows-<arch>/
//!   bin/         nox.exe, nox-cli.exe and the DLLs they load
//!   lib/         the SVG pixbuf loader and its cache
//!   share/       translations (Nox's, GTK's), icons (Nox's, Adwaita),
//!                GTK's compiled GSettings schemas
//! ```
//!
//! GTK on Windows finds its data relative to its DLLs (`bin/../share`), so
//! no launcher is needed. DLLs not in the MSYS2 prefix belong to Windows or
//! the graphics driver (`vulkan-1.dll`, `nvcuda.dll`) and aren't bundled.
//! The same tree is the input of the installer (`packaging/windows`).

// Compiled everywhere (so it's checked everywhere), used on its platform.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Layout, Result, VERSION, copy, install, output, zip_dir};

/// DLLs that come from the system even if MSYS2 has a copy.
const SYSTEM_DLLS: &[&str] = &["vulkan-1.dll", "nvcuda.dll"];

/// Builds the package tree and its archive in `dist`; returns both.
pub fn dist(dist: &Path) -> Result<(PathBuf, PathBuf)> {
    let prefix = PathBuf::from(
        std::env::var_os("MSYSTEM_PREFIX")
            .ok_or("run in an MSYS2 shell (CLANG64 or CLANGARM64): MSYSTEM_PREFIX isn't set")?,
    );
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let name = format!("nox-{VERSION}-windows-{arch}");
    let stage = dist.join(&name);
    crate::remove_dir(&stage)?;
    install(&stage, Layout::Portable)?;
    let bin = stage.join("bin");
    let prefix_bin = prefix.join("bin");

    // The SVG pixbuf loader (icons).
    let loaders_rel = Path::new("lib/gdk-pixbuf-2.0/2.10.0/loaders");
    let mut loaders = Vec::new();
    for entry in std::fs::read_dir(prefix.join(loaders_rel)).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if file.contains("svg") && file.ends_with(".dll") {
            let target = stage
                .join(loaders_rel)
                .join(path.file_name().expect("file"));
            copy(&path, &target)?;
            loaders.push(target);
        }
    }

    // Every DLL they load from the prefix, transitively.
    let mut objects: Vec<PathBuf> = crate::PROGRAMS
        .iter()
        .map(|p| bin.join(format!("{p}.exe")))
        .chain(loaders.iter().cloned())
        .collect();
    let mut bundled = BTreeSet::new();
    let mut copied = 0;
    while let Some(object) = objects.pop() {
        for dll in imported_dlls(&object)? {
            let lower = dll.to_lowercase();
            if SYSTEM_DLLS.contains(&lower.as_str()) || !bundled.insert(lower) {
                continue;
            }
            if let Some(found) = find_dll(&prefix_bin, &dll) {
                let target = bin.join(found.file_name().expect("file"));
                copy(&found, &target)?;
                objects.push(target);
                copied += 1;
            }
        }
    }
    println!("xtask: bundled {copied} DLLs");

    // GLib's helper for spawning processes.
    for helper in ["gspawn-win64-helper.exe", "gspawn-win64-helper-console.exe"] {
        if prefix_bin.join(helper).exists() {
            copy(&prefix_bin.join(helper), &bin.join(helper))?;
        }
    }

    // The loader cache, with paths relative to the package (gdk-pixbuf
    // resolves them against its installation).
    if !loaders.is_empty() {
        let mut query = Command::new(prefix_bin.join("gdk-pixbuf-query-loaders.exe"));
        query.args(&loaders);
        let cache = output(&mut query)?;
        let stage_text = stage.to_string_lossy().replace('\\', "/");
        let cache = cache
            .replace('\\', "/")
            .replace(&format!("{stage_text}/"), "");
        std::fs::write(
            stage
                .join(loaders_rel)
                .parent()
                .expect("dir")
                .join("loaders.cache"),
            cache,
        )
        .map_err(|e| e.to_string())?;
    }

    // GTK's schemas, icons and translations.
    let share = prefix.join("share");
    crate::bundle_gtk_data(
        &share,
        &stage.join("share"),
        &prefix_bin.join("glib-compile-schemas.exe"),
    )?;
    copy_system_translations(&share.join("locale"), &stage.join("share/locale"))?;

    std::fs::write(stage.join("README.txt"), README.replace('\n', "\r\n"))
        .map_err(|e| e.to_string())?;
    copy(&crate::root().join("LICENSE"), &stage.join("LICENSE.txt"))?;

    let archive = dist.join(format!("{name}.zip"));
    zip_dir(&stage, &archive)?;
    Ok((stage, archive))
}

/// GTK's and GLib's translations, for the languages Nox has.
fn copy_system_translations(from: &Path, to: &Path) -> Result {
    let ours: BTreeSet<String> = std::fs::read_dir(to)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    for lang in ours {
        for domain in ["gtk40", "glib20"] {
            let file = from
                .join(&lang)
                .join("LC_MESSAGES")
                .join(format!("{domain}.mo"));
            if file.exists() {
                copy(
                    &file,
                    &to.join(&lang)
                        .join("LC_MESSAGES")
                        .join(format!("{domain}.mo")),
                )?;
            }
        }
    }
    Ok(())
}

/// `name` in `dir`, ignoring case (as Windows does).
fn find_dll(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        return Some(exact);
    }
    let lower = name.to_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|e| (e.file_name().to_string_lossy().to_lowercase() == lower).then(|| e.path()))
}

/// The DLLs a PE file imports (normal and delay-loaded imports).
pub fn imported_dlls(path: &Path) -> Result<Vec<String>> {
    let data = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    parse_imports(&data).ok_or_else(|| format!("{}: not a PE file", path.display()))
}

fn parse_imports(data: &[u8]) -> Option<Vec<String>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    if data.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(coff + 2)? as usize;
    let optional_size = u16_at(coff + 16)? as usize;
    let optional = coff + 20;
    // Data directories: PE32+ (64-bit) or PE32.
    let directories = match u16_at(optional)? {
        0x20b => optional + 112,
        0x10b => optional + 96,
        _ => return None,
    };
    let section_table = optional + optional_size;
    // RVA → file offset, through the section headers.
    let offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let header = section_table + i * 40;
            let size = u32_at(header + 8)?.max(u32_at(header + 16)?);
            let address = u32_at(header + 12)?;
            let raw = u32_at(header + 20)?;
            (rva >= address && rva < address + size).then(|| (rva - address + raw) as usize)
        })
    };
    let name_at = |rva: u32| -> Option<String> {
        let start = offset(rva)?;
        let end = start + data.get(start..)?.iter().position(|&b| b == 0)?;
        Some(String::from_utf8_lossy(&data[start..end]).into_owned())
    };
    let mut dlls = Vec::new();
    // Import descriptors (20 bytes, name RVA at 12), then delay-load
    // descriptors (32 bytes, name RVA at 4); both end with a zero entry.
    for (directory, size, name_field) in [(1, 20, 12), (13, 32, 4)] {
        let rva = u32_at(directories + directory * 8)?;
        if rva == 0 {
            continue;
        }
        let Some(mut at) = offset(rva) else { continue };
        loop {
            let name = u32_at(at + name_field)?;
            if name == 0 {
                break;
            }
            dlls.push(name_at(name)?);
            at += size;
        }
    }
    Some(dlls)
}

const README: &str = "\
Nox for Windows

Start bin\\nox.exe for the editor, or use bin\\nox-cli.exe from a terminal.
Everything Nox needs comes with it, except the graphics driver (with
Vulkan 1.3).

Nox is free software under the GNU GPL, version 3 or later (LICENSE.txt).
It includes GTK, FFmpeg (with x264 and x265) and other libraries from
MSYS2; see share\\licenses for the licenses of the Rust crates.
";

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE32+ file importing one DLL.
    #[test]
    fn reads_imports() {
        let mut pe = vec![0u8; 0x400];
        pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        pe[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // sections
        pe[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes()); // optional size
        let optional = coff + 20;
        pe[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        let directories = optional + 112;
        // Import directory at RVA 0x1000.
        pe[directories + 8..directories + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        // One section: RVA 0x1000, size 0x200, raw data at 0x200.
        let section = optional + 240;
        pe[section + 8..section + 12].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        pe[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
        // Descriptor at 0x200 (RVA 0x1000) naming RVA 0x1100 (file 0x300).
        pe[0x200 + 12..0x200 + 16].copy_from_slice(&0x1100u32.to_le_bytes());
        pe[0x300..0x30a].copy_from_slice(b"gtk-4.dll\0");
        assert_eq!(parse_imports(&pe), Some(vec!["gtk-4.dll".to_owned()]));
    }
}
