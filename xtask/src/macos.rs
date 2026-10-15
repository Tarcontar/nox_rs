//! The macOS app bundle, built with Homebrew's GTK, librsvg, codec
//! libraries, Vulkan loader and MoltenVK (FFmpeg from
//! `packaging/ffmpeg/build.sh`):
//!
//! ```text
//! Nox.app/Contents/
//!   Info.plist
//!   MacOS/Nox, MacOS/nox-cli   launchers: set GTK's and Vulkan's paths
//!   Resources/bin/             nox, nox-cli, gdk-pixbuf-query-loaders
//!   Resources/lib/             the SVG pixbuf loader
//!   Resources/share/           translations, icons, GSettings schemas
//!   Resources/vulkan/icd.d/    MoltenVK's driver manifest
//!   Resources/Nox.icns
//!   Frameworks/                every non-system dylib, as @rpath/<name>
//! ```
//!
//! Homebrew's libraries name each other by absolute paths; they're copied
//! into `Frameworks` and relinked (`install_name_tool`), then everything
//! is signed ad hoc (Apple Silicon runs only signed code). Without a
//! Developer ID, Gatekeeper still asks before the first start. The
//! launchers are shell scripts because the Vulkan loader is opened by name
//! (`dlopen("libvulkan.1.dylib")`, by ash and FFmpeg), which only
//! `DYLD_LIBRARY_PATH` can point into the bundle.
//!
//! Needs Xcode's command line tools (otool, install_name_tool, codesign,
//! iconutil, ditto) and rsvg-convert.

// Compiled everywhere (so it's checked everywhere), used on its platform.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{APP_ID, Layout, Result, VERSION, copy, install, output, run};

/// Builds `Nox.app` and its archive in `dist`; returns the archive.
pub fn dist(dist: &Path) -> Result<PathBuf> {
    let brew = PathBuf::from(output(Command::new("brew").arg("--prefix"))?.trim());
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    let stage = dist.join(format!("nox-{VERSION}-macos-{arch}"));
    crate::remove_dir(&stage)?;
    let app = stage.join("Nox.app");
    let contents = app.join("Contents");
    let resources = contents.join("Resources");
    let frameworks = contents.join("Frameworks");
    install(&resources, Layout::Portable)?;

    // The SVG pixbuf loader and the tool that registers it.
    let loaders_rel = Path::new("lib/gdk-pixbuf-2.0/2.10.0/loaders");
    let brew_loaders = brew.join(loaders_rel);
    let mut objects: Vec<PathBuf> = crate::PROGRAMS
        .iter()
        .map(|p| resources.join("bin").join(p))
        .collect();
    for entry in std::fs::read_dir(&brew_loaders).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path
            .file_name()
            .is_some_and(|f| f.to_string_lossy().contains("svg"))
        {
            let target = resources
                .join(loaders_rel)
                .join(path.file_name().expect("file"));
            copy(&path, &target)?;
            make_writable(&target)?;
            objects.push(target);
        }
    }
    let query = resources.join("bin/gdk-pixbuf-query-loaders");
    copy(&brew.join("bin/gdk-pixbuf-query-loaders"), &query)?;
    make_writable(&query)?;
    objects.push(query);

    // Vulkan: the loader and MoltenVK, with its driver manifest.
    for library in ["libvulkan.1.dylib", "libMoltenVK.dylib"] {
        let path = std::fs::canonicalize(brew.join("lib").join(library))
            .map_err(|err| format!("{library}: {err} (brew install vulkan-loader molten-vk)"))?;
        let target = frameworks.join(library);
        copy(&path, &target)?;
        make_writable(&target)?;
        objects.push(target);
    }
    let icd = resources.join("vulkan/icd.d/MoltenVK_icd.json");
    std::fs::create_dir_all(icd.parent().expect("dir")).map_err(|e| e.to_string())?;
    std::fs::write(
        &icd,
        r#"{
    "file_format_version": "1.0.0",
    "ICD": {
        "library_path": "../../../Frameworks/libMoltenVK.dylib",
        "api_version": "1.4.0",
        "is_portability_driver": true
    }
}
"#,
    )
    .map_err(|e| e.to_string())?;

    // Every dylib they load, copied and relinked.
    let mut bundled: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut queue = objects.clone();
    let mut seen = BTreeSet::new();
    while let Some(object) = queue.pop() {
        if !seen.insert(object.clone()) {
            continue;
        }
        for (reference, path) in dependencies(&object)? {
            let name = file_name(&path);
            let target = frameworks.join(&name);
            if !bundled.contains_key(&name) && !target.exists() {
                copy(&path, &target)?;
                make_writable(&target)?;
                bundled.insert(name.clone(), target.clone());
                queue.push(target.clone());
            }
            run(Command::new("install_name_tool")
                .args(["-change", &reference, &format!("@rpath/{name}")])
                .arg(&object))?;
        }
    }
    println!("xtask: bundled {} libraries", bundled.len());
    for library in std::fs::read_dir(&frameworks).map_err(|e| e.to_string())? {
        let library = library.map_err(|e| e.to_string())?.path();
        run(Command::new("install_name_tool")
            .args(["-id", &format!("@rpath/{}", file_name(&library))])
            .arg(&library))?;
        add_rpath(&library, "@loader_path")?;
    }
    for object in &objects {
        if object.starts_with(&frameworks) {
            continue;
        }
        let depth = object
            .parent()
            .and_then(|dir| dir.strip_prefix(&contents).ok())
            .map_or(0, |rel| rel.components().count());
        add_rpath(
            object,
            &format!("@loader_path/{}Frameworks", "../".repeat(depth)),
        )?;
    }

    // GTK's schemas and icons.
    crate::bundle_gtk_data(
        &brew.join("share"),
        &resources.join("share"),
        &brew.join("bin/glib-compile-schemas"),
    )?;

    // The app: icon, Info.plist, launchers.
    make_icns(&resources.join("Nox.icns"))?;
    std::fs::write(contents.join("Info.plist"), info_plist()).map_err(|e| e.to_string())?;
    for (launcher, program) in [("Nox", "nox"), ("nox-cli", "nox-cli")] {
        write_launcher(&contents.join("MacOS").join(launcher), program)?;
    }

    // Sign every binary (relinking invalidated their signatures), then the
    // app. codesign counts everything in MacOS but the main executable as
    // nested code, so the nox-cli launcher script needs its own signature.
    let mut binaries: Vec<PathBuf> = std::fs::read_dir(&frameworks)
        .map_err(|e| e.to_string())?
        .filter_map(|e| Some(e.ok()?.path()))
        .collect();
    binaries.extend(
        objects
            .iter()
            .filter(|o| !o.starts_with(&frameworks))
            .cloned(),
    );
    binaries.push(contents.join("MacOS/nox-cli"));
    for binary in &binaries {
        run(Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(binary))?;
    }
    run(Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app))?;

    std::fs::write(stage.join("README.txt"), README).map_err(|e| e.to_string())?;
    copy(&crate::root().join("LICENSE"), &stage.join("LICENSE.txt"))?;
    let archive = dist.join(format!("nox-{VERSION}-macos-{arch}.zip"));
    if archive.exists() {
        std::fs::remove_file(&archive).map_err(|e| e.to_string())?;
    }
    // ditto keeps the signatures' extended attributes, unlike zip.
    run(Command::new("ditto")
        .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
        .arg(&stage)
        .arg(&archive))?;
    Ok(archive)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The non-system libraries `object` links: (reference as written, file).
fn dependencies(object: &Path) -> Result<Vec<(String, PathBuf)>> {
    let listing = output(Command::new("otool").arg("-L").arg(object))?;
    let rpaths = rpaths(object)?;
    let loader = object.parent().unwrap_or(Path::new("."));
    let own_id = output(Command::new("otool").arg("-D").arg(object))?;
    let own_id = own_id
        .lines()
        .nth(1)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let mut found = Vec::new();
    // Line 1 names the object; then "\t<reference> (compatibility …)".
    for line in listing.lines().skip(1) {
        let Some(reference) = line.trim().split(" (").next() else {
            continue;
        };
        if reference.is_empty()
            || reference == own_id
            || reference.starts_with("/usr/lib/")
            || reference.starts_with("/System/")
        {
            continue;
        }
        let path = if let Some(rest) = reference.strip_prefix("@rpath/") {
            rpaths.iter().map(|dir| dir.join(rest)).find(|p| p.exists())
        } else if let Some(rest) = reference.strip_prefix("@loader_path/") {
            Some(loader.join(rest))
        } else {
            Some(PathBuf::from(reference))
        };
        let Some(path) = path.filter(|p| p.exists()) else {
            // Already relinked into the bundle.
            if reference.starts_with("@rpath/") {
                continue;
            }
            return Err(format!("{}: can't find {reference}", object.display()));
        };
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        found.push((reference.to_owned(), path));
    }
    Ok(found)
}

/// An object's `LC_RPATH` entries, with `@loader_path` resolved.
fn rpaths(object: &Path) -> Result<Vec<PathBuf>> {
    let commands = output(Command::new("otool").arg("-l").arg(object))?;
    let loader = object.parent().unwrap_or(Path::new("."));
    let mut paths = Vec::new();
    let mut in_rpath = false;
    for line in commands.lines().map(str::trim) {
        if line.starts_with("cmd ") {
            in_rpath = line == "cmd LC_RPATH";
        } else if in_rpath && let Some(rest) = line.strip_prefix("path ") {
            let path = rest.split(" (offset").next().unwrap_or(rest);
            paths.push(match path.strip_prefix("@loader_path") {
                Some(rest) => loader.join(rest.trim_start_matches('/')),
                None => PathBuf::from(path),
            });
        }
    }
    Ok(paths)
}

fn add_rpath(object: &Path, rpath: &str) -> Result {
    // Fails if it's there already, which is fine.
    let _ = Command::new("install_name_tool")
        .args(["-add_rpath", rpath])
        .arg(object)
        .output();
    Ok(())
}

/// Homebrew installs libraries read-only.
fn make_writable(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    std::fs::set_permissions(path, permissions).map_err(|e| e.to_string())
}

/// Renders the app icon at the sizes macOS wants and packs them.
fn make_icns(target: &Path) -> Result {
    let iconset = target.with_extension("iconset");
    std::fs::create_dir_all(&iconset).map_err(|e| e.to_string())?;
    let svg = crate::root().join(format!("data/icons/hicolor/scalable/apps/{APP_ID}.svg"));
    for size in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let suffix = if scale == 2 { "@2x" } else { "" };
            let pixels = (size * scale).to_string();
            run(Command::new("rsvg-convert")
                .args(["-w", &pixels, "-h", &pixels, "-o"])
                .arg(iconset.join(format!("icon_{size}x{size}{suffix}.png")))
                .arg(&svg))?;
        }
    }
    run(Command::new("iconutil")
        .args(["-c", "icns", "-o"])
        .arg(target)
        .arg(&iconset))?;
    std::fs::remove_dir_all(&iconset).map_err(|e| e.to_string())
}

fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>{APP_ID}</string>
    <key>CFBundleName</key><string>Nox</string>
    <key>CFBundleDisplayName</key><string>Nox</string>
    <key>CFBundleExecutable</key><string>Nox</string>
    <key>CFBundleIconFile</key><string>Nox</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{VERSION}</string>
    <key>CFBundleVersion</key><string>{VERSION}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>OpenTimelineIO project</string>
            <key>CFBundleTypeRole</key><string>Editor</string>
            <key>CFBundleTypeExtensions</key><array><string>otio</string></array>
        </dict>
    </array>
</dict>
</plist>
"#
    )
}

fn write_launcher(path: &Path, program: &str) -> Result {
    let script = format!(
        r#"#!/bin/sh
# Runs {program} with the libraries and data in this app bundle.
contents="$(cd "$(dirname "$0")/.." && pwd)"
res="$contents/Resources"
export XDG_DATA_DIRS="$res/share:${{XDG_DATA_DIRS:-/usr/local/share:/usr/share}}"
export GSETTINGS_SCHEMA_DIR="$res/share/glib-2.0/schemas"
export GTK_EXE_PREFIX="$res" GTK_DATA_PREFIX="$res"
export GSETTINGS_BACKEND="${{GSETTINGS_BACKEND:-memory}}"
export GDK_PIXBUF_MODULEDIR="$res/lib/gdk-pixbuf-2.0/2.10.0/loaders"
cache="$HOME/Library/Caches/{APP_ID}"
mkdir -p "$cache"
export GDK_PIXBUF_MODULE_FILE="$cache/loaders.cache"
"$res/bin/gdk-pixbuf-query-loaders" > "$GDK_PIXBUF_MODULE_FILE" 2>/dev/null
# The Vulkan loader is opened by name; MoltenVK is its driver.
export DYLD_LIBRARY_PATH="$contents/Frameworks"
export VK_DRIVER_FILES="$res/vulkan/icd.d/MoltenVK_icd.json"
export VK_ICD_FILENAMES="$VK_DRIVER_FILES"
exec "$res/bin/{program}" "$@"
"#
    );
    std::fs::create_dir_all(path.parent().expect("dir")).map_err(|e| e.to_string())?;
    crate::write_executable(path, &script)
}

const README: &str = "\
Nox for macOS (Apple Silicon)

Move Nox.app to Applications and open it. Nox isn't signed with an Apple
Developer ID yet, so macOS asks before the first start: right-click
Nox.app, choose Open, and confirm (or run `xattr -cr Nox.app`).

The command-line tool is Nox.app/Contents/MacOS/nox-cli.

Nox is free software under the GNU GPL, version 3 or later (LICENSE.txt).
It includes GTK, FFmpeg (with x264 and x265), MoltenVK and other libraries
from Homebrew; see Nox.app/Contents/Resources/share/licenses for the
licenses of the Rust crates.
";
