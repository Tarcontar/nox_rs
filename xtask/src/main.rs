//! Build and packaging tasks, run as `cargo run --package xtask -- <task>`
//! (`cargo xtask <task>` with the alias in `.cargo/config.toml`):
//!
//! - `install --prefix DIR [--no-build]`: builds Nox (release) and installs
//!   it into `DIR` in the usual layout: `bin/nox`, `bin/nox-cli`, and in
//!   `share/` the translations, icons, desktop entry, AppStream metainfo,
//!   MIME type and license. Flatpak builds use this (`--prefix /app`).
//! - `dist [--no-build]`: a self-contained package of Nox and the native
//!   libraries it needs, for this platform, as a ZIP archive in
//!   `target/dist` (see `linux.rs`, `windows.rs`, `macos.rs`).
//!
//! - `flatpak-sources`: regenerates `packaging/flatpak/cargo-sources.json`
//!   from `Cargo.lock` (see `flatpak.rs`); run it after dependencies change.
//!
//! `--no-build` uses the release binaries already built.

mod flatpak;
mod linux;
#[cfg(unix)]
mod macos;
#[path = "../../crates/nox-gui/po.rs"]
mod po;
mod windows;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The application ID: desktop entry, metainfo, icons.
pub const APP_ID: &str = "io.github.feliwir.Nox";
/// Nox's version (the workspace's).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The programs installed.
pub const PROGRAMS: [&str; 2] = ["nox", "nox-cli"];

type Result<T = ()> = std::result::Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("install") => install_task(&args[1..]),
        Some("dist") => dist_task(&args[1..]),
        Some("flatpak-sources") => flatpak::sources(),
        _ => Err(
            "usage: cargo xtask install --prefix DIR [--no-build]\n       cargo xtask dist [--no-build]\n       cargo xtask flatpak-sources"
                .into(),
        ),
    };
    if let Err(err) = result {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}

fn install_task(args: &[String]) -> Result {
    let mut prefix = None;
    let mut build = true;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--prefix" => {
                prefix = Some(PathBuf::from(rest.next().ok_or("--prefix needs a value")?))
            }
            "--no-build" => build = false,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let prefix = prefix.ok_or("install needs --prefix")?;
    if build {
        build_release()?;
    }
    install(&prefix, Layout::Desktop)
}

fn dist_task(args: &[String]) -> Result {
    let mut build = true;
    for arg in args {
        match arg.as_str() {
            "--no-build" => build = false,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if build {
        build_release()?;
    }
    let dist = target_dir().join("dist");
    #[cfg(target_os = "linux")]
    let archive = linux::dist(&dist)?;
    #[cfg(target_os = "macos")]
    let archive = macos::dist(&dist)?;
    #[cfg(windows)]
    let archive = windows::dist(&dist)?.1;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let archive: PathBuf = return Err("dist isn't implemented for this platform".into());
    println!("{}", archive.display());
    Ok(())
}

/// The workspace root.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is in the workspace")
        .to_path_buf()
}

pub fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root().join("target"), PathBuf::from)
}

/// Builds the programs in release mode.
fn build_release() -> Result {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(root())
        .args(["build", "--release", "--locked"]);
    for package in ["nox-gui", "nox-cli"] {
        command.args(["--package", package]);
    }
    run(&mut command)
}

/// What else goes into an install besides the programs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Desktop integration for Linux: desktop entry, metainfo, MIME type,
    /// icon theme entries.
    Desktop,
    /// The programs, translations and license only.
    Portable,
}

/// Installs the release programs and Nox's data into `prefix`.
pub fn install(prefix: &Path, layout: Layout) -> Result {
    let root = root();
    let release = target_dir().join("release");
    for program in PROGRAMS {
        let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
        copy(&release.join(&name), &prefix.join("bin").join(&name))?;
    }
    let share = prefix.join("share");
    po::compile_catalogs(&root.join("po"), &share.join("locale"))?;
    let licenses = share.join("licenses").join("nox");
    copy(&root.join("LICENSE"), &licenses.join("LICENSE"))?;
    crate_licenses(&licenses.join("crates"))?;
    let icons = root.join("data/icons/hicolor");
    for icon in [
        format!("scalable/apps/{APP_ID}.svg"),
        format!("symbolic/apps/{APP_ID}-symbolic.svg"),
    ] {
        copy(&icons.join(&icon), &share.join("icons/hicolor").join(&icon))?;
    }
    if layout == Layout::Desktop {
        let data = root.join("data");
        copy(
            &data.join(format!("{APP_ID}.desktop")),
            &share.join("applications").join(format!("{APP_ID}.desktop")),
        )?;
        copy(
            &data.join(format!("{APP_ID}.metainfo.xml")),
            &share
                .join("metainfo")
                .join(format!("{APP_ID}.metainfo.xml")),
        )?;
        copy(
            &data.join(format!("{APP_ID}.mime.xml")),
            &share.join("mime/packages").join(format!("{APP_ID}.xml")),
        )?;
    }
    Ok(())
}

/// Writes the licenses of the Rust crates the programs are built from:
/// `crates.txt` (name, version, license) and each crate's license files.
fn crate_licenses(dir: &Path) -> Result {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let metadata = output(
        Command::new(cargo)
            .current_dir(root())
            .args(["metadata", "--format-version", "1", "--locked", "--offline"])
            .args(["--filter-platform", &host_triple()?]),
    )?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata).map_err(|err| format!("cargo metadata: {err}"))?;
    // The crates the programs depend on: the resolved graph from nox-gui
    // and nox-cli, without build-only and dev-only dependencies' own.
    let nodes: BTreeMap<&str, &serde_json::Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("cargo metadata: no resolve graph")?
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?, node)))
        .collect();
    let packages: BTreeMap<&str, &serde_json::Value> = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?
        .iter()
        .filter_map(|package| Some((package["id"].as_str()?, package)))
        .collect();
    let mut stack: Vec<&str> = packages
        .iter()
        .filter(|(_, p)| matches!(p["name"].as_str(), Some("nox-gui" | "nox-cli")))
        .map(|(id, _)| *id)
        .collect();
    let mut used = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !used.insert(id) {
            continue;
        }
        let Some(node) = nodes.get(id) else { continue };
        for dep in node["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind["kind"].is_null());
            if normal && let Some(dep_id) = dep["pkg"].as_str() {
                stack.push(dep_id);
            }
        }
    }
    let mut list = String::from(
        "Rust crates built into Nox, with their licenses (license texts in this folder):\n\n",
    );
    for id in used {
        let package = packages[id];
        let (Some(name), Some(version)) = (package["name"].as_str(), package["version"].as_str())
        else {
            continue;
        };
        if package["source"].is_null() {
            continue; // Nox's own crates
        }
        let license = package["license"].as_str().unwrap_or("see license files");
        list += &format!("{name} {version}: {license}\n");
        let manifest = PathBuf::from(package["manifest_path"].as_str().unwrap_or_default());
        let Some(crate_dir) = manifest.parent() else {
            continue;
        };
        let entries = std::fs::read_dir(crate_dir).map_err(|e| e.to_string())?;
        for entry in entries.filter_map(std::result::Result::ok) {
            let file = entry.file_name().to_string_lossy().to_uppercase();
            if (file.starts_with("LICENSE")
                || file.starts_with("COPYING")
                || file.starts_with("NOTICE"))
                && entry.path().is_file()
            {
                copy(
                    &entry.path(),
                    &dir.join(format!("{name}-{version}"))
                        .join(entry.file_name()),
                )?;
            }
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("crates.txt"), list).map_err(|e| e.to_string())?;
    Ok(())
}

/// The host's target triple, from rustc.
fn host_triple() -> Result<String> {
    let info = output(Command::new("rustc").arg("-vV"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV: no host".into())
}

/// Copies a file, creating the directories it goes in, keeping its
/// permissions.
/// GTK's data a package needs, from `share` (GTK's installation) into
/// `dest` (the package's `share`): its settings schemas (file chooser and
/// others), compiled with `compile_schemas`, and its icons (Adwaita and
/// hicolor's index).
pub fn bundle_gtk_data(share: &Path, dest: &Path, compile_schemas: &Path) -> Result {
    let schemas = dest.join("glib-2.0/schemas");
    for entry in std::fs::read_dir(share.join("glib-2.0/schemas")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if file.starts_with("org.gtk.gtk4.") && file.ends_with(".gschema.xml") {
            copy(&path, &schemas.join(&file))?;
        }
    }
    run(Command::new(compile_schemas).arg(&schemas))?;
    copy_tree(&share.join("icons/Adwaita"), &dest.join("icons/Adwaita"))?;
    copy(
        &share.join("icons/hicolor/index.theme"),
        &dest.join("icons/hicolor/index.theme"),
    )
}

/// Removes `dir` with its contents, if it exists (a fresh stage).
pub fn remove_dir(dir: &Path) -> Result {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    Ok(())
}

/// Writes a script and makes it executable (on Unix).
pub fn write_executable(path: &Path, script: &str) -> Result {
    std::fs::write(path, script).map_err(|err| format!("{}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("{}: {err}", path.display()))?;
    }
    Ok(())
}

pub fn copy(from: &Path, to: &Path) -> Result {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    std::fs::copy(from, to)
        .map_err(|err| format!("{} → {}: {err}", from.display(), to.display()))?;
    Ok(())
}

/// Copies a directory tree.
pub fn copy_tree(from: &Path, to: &Path) -> Result {
    let entries = std::fs::read_dir(from).map_err(|err| format!("{}: {err}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Runs a command, failing if it does.
pub fn run(command: &mut Command) -> Result {
    let status = command
        .status()
        .map_err(|err| format!("{:?}: {err}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed ({status})"))
    }
}

/// Runs a command and returns its standard output.
pub fn output(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .map_err(|err| format!("{:?}: {err}", command.get_program()))?;
    if !output.status.success() {
        return Err(format!(
            "{command:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Writes `dir` into a ZIP archive at `archive`, under its own name, keeping
/// Unix permissions (so programs stay executable).
pub fn zip_dir(dir: &Path, archive: &Path) -> Result {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let file =
        std::fs::File::create(archive).map_err(|err| format!("{}: {err}", archive.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let base = dir.parent().expect("not the root");
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&current)
            .map_err(|err| format!("{}: {err}", current.display()))?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .strip_prefix(base)
                .expect("inside")
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path)
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode();
                options = options.unix_permissions(mode);
            }
            if path.is_dir() {
                zip.add_directory(format!("{name}/"), options)
                    .map_err(|e| e.to_string())?;
                stack.push(path);
            } else {
                zip.start_file(name, options).map_err(|e| e.to_string())?;
                let data =
                    std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
                zip.write_all(&data).map_err(|e| e.to_string())?;
            }
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}
