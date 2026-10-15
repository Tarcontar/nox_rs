//! The self-contained Linux package: Nox with the libraries it links
//! (GTK, FFmpeg and their dependencies) and GTK's data, in a ZIP archive:
//!
//! ```text
//! nox-<version>-linux-<arch>/
//!   nox, nox-cli        launchers (set GTK's data paths, run bin/)
//!   bin/                the programs (rpath $ORIGIN/../lib)
//!   lib/                bundled libraries (rpath $ORIGIN), the SVG
//!                       pixbuf loader and gdk-pixbuf-query-loaders
//!   share/              translations, icons (Nox's, Adwaita), GTK's
//!                       compiled GSettings schemas, desktop entry
//! ```
//!
//! Libraries every desktop system has are left out (glibc, the C++
//! runtime, graphics drivers and their loaders, X11/Wayland, ALSA,
//! fontconfig), as in AppImage's exclude list: bundled copies of those
//! clash with the system's drivers and configuration. The package runs on
//! systems with a glibc at least as new as the build system's, so it's
//! built on an older distribution (Debian 13). Needs `patchelf` and
//! `glib-compile-schemas`.

// Compiled everywhere (so it's checked everywhere), used on its platform.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Layout, Result, VERSION, copy, install, output, run, zip_dir};

/// Library name prefixes (sonames) not bundled.
const SYSTEM_LIBRARIES: &[&str] = &[
    // glibc and the toolchain runtime
    "ld-linux",
    "libc.so",
    "libm.so",
    "libdl.so",
    "libpthread.so",
    "librt.so",
    "libresolv.so",
    "libutil.so",
    "libanl.so",
    "libmvec.so",
    "libstdc++.so",
    "libgcc_s.so",
    // graphics drivers and their loaders
    "libGL.so",
    "libGLX",
    "libEGL",
    "libGLdispatch",
    "libOpenGL",
    "libGLES",
    "libvulkan.so",
    "libdrm.so",
    "libgbm.so",
    "libcuda.so",
    "libnvidia-",
    // display servers
    "libX11.so",
    "libX11-xcb",
    "libxcb.so",
    "libxshmfence",
    "libwayland-",
    // system services and configuration
    "libasound.so",
    "libfontconfig.so",
    "libfreetype.so",
    "libharfbuzz.so",
    "libz.so",
    "libexpat.so",
    "libdbus-1.so",
    "libudev.so",
    "libsystemd.so",
];

/// Builds the package in `dist` and returns the archive.
pub fn dist(dist: &Path) -> Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    let name = format!("nox-{VERSION}-linux-{arch}");
    let stage = dist.join(&name);
    crate::remove_dir(&stage)?;
    install(&stage, Layout::Desktop)?;
    let lib = stage.join("lib");

    // The SVG loader (icons) and the tool that registers it.
    let loaders_dir = PathBuf::from(pkg_config("gdk-pixbuf-2.0", "gdk_pixbuf_moduledir")?);
    let query_loaders = PathBuf::from(pkg_config("gdk-pixbuf-2.0", "gdk_pixbuf_query_loaders")?);
    let bundled_loaders = lib.join("gdk-pixbuf-2.0/2.10.0/loaders");
    let svg = loaders_dir.join("libpixbufloader_svg.so");
    let loaders: Vec<PathBuf> = if svg.exists() {
        copy(&svg, &bundled_loaders.join("libpixbufloader_svg.so"))?;
        vec![bundled_loaders.join("libpixbufloader_svg.so")]
    } else {
        eprintln!("xtask: no SVG pixbuf loader at {}", svg.display());
        Vec::new()
    };
    let bundled_query = lib.join("nox/gdk-pixbuf-query-loaders");
    copy(&query_loaders, &bundled_query)?;

    // Every library they need, found by the dynamic linker.
    let programs: Vec<PathBuf> = crate::PROGRAMS
        .iter()
        .map(|p| stage.join("bin").join(p))
        .collect();
    let mut needed = BTreeMap::new();
    for object in programs.iter().chain(&loaders).chain([&bundled_query]) {
        for (soname, path) in shared_libraries(object)? {
            needed.entry(soname).or_insert(path);
        }
    }
    for (soname, path) in &needed {
        copy(path, &lib.join(soname))?;
    }
    println!("xtask: bundled {} libraries", needed.len());
    copy_copyrights(needed.values(), &stage.join("share/doc"))?;

    // Find them next to each other rather than in the system.
    for program in &programs {
        set_rpath(program, "$ORIGIN/../lib")?;
    }
    for soname in needed.keys() {
        set_rpath(&lib.join(soname), "$ORIGIN")?;
    }
    for loader in &loaders {
        set_rpath(loader, "$ORIGIN/../../..")?;
    }
    set_rpath(&bundled_query, "$ORIGIN/..")?;

    // GTK's settings schemas and icons.
    let gtk_share = PathBuf::from(pkg_config("gtk4", "prefix")?).join("share");
    crate::bundle_gtk_data(
        &gtk_share,
        &stage.join("share"),
        Path::new("glib-compile-schemas"),
    )?;

    for program in crate::PROGRAMS {
        write_launcher(&stage.join(program), program)?;
    }
    std::fs::write(stage.join("README.txt"), README).map_err(|e| e.to_string())?;
    copy(&crate::root().join("LICENSE"), &stage.join("LICENSE"))?;

    let archive = dist.join(format!("{name}.zip"));
    zip_dir(&stage, &archive)?;
    Ok(archive)
}

/// Copies the Debian copyright file (licenses) of each library's package
/// to `doc/<package>/copyright`, if the system is Debian-based.
fn copy_copyrights<'a>(libraries: impl Iterator<Item = &'a PathBuf>, doc: &Path) -> Result {
    let mut packages = std::collections::BTreeSet::new();
    for library in libraries {
        // dpkg knows the path it installed, which may be a symlink's.
        let real = std::fs::canonicalize(library).unwrap_or_else(|_| library.clone());
        for path in [library, &real] {
            if let Ok(found) = output(Command::new("dpkg-query").arg("--search").arg(path)) {
                if let Some(package) = found.split(':').next() {
                    packages.insert(package.trim().to_owned());
                }
                break;
            }
        }
    }
    if packages.is_empty() {
        eprintln!("xtask: no dpkg; library licenses not included");
    }
    for package in packages {
        let copyright = Path::new("/usr/share/doc").join(&package).join("copyright");
        if copyright.exists() {
            copy(&copyright, &doc.join(&package).join("copyright"))?;
        }
    }
    Ok(())
}

fn pkg_config(package: &str, variable: &str) -> Result<String> {
    let value = output(
        Command::new("pkg-config")
            .arg(format!("--variable={variable}"))
            .arg(package),
    )?;
    Ok(value.trim().to_owned())
}

/// The shared libraries `object` loads (transitively), by soname, except
/// the system's own.
fn shared_libraries(object: &Path) -> Result<Vec<(String, PathBuf)>> {
    let listing = output(Command::new("ldd").arg(object))?;
    let mut libraries = Vec::new();
    for line in listing.lines() {
        // "\tlibfoo.so.1 => /usr/lib/x86_64-linux-gnu/libfoo.so.1 (0x…)"
        let Some((soname, rest)) = line.trim().split_once(" => ") else {
            continue;
        };
        if rest.starts_with("not found") {
            return Err(format!("{}: {soname} not found", object.display()));
        }
        let Some(path) = rest.split(" (").next().filter(|p| p.starts_with('/')) else {
            continue;
        };
        if SYSTEM_LIBRARIES
            .iter()
            .any(|prefix| soname.starts_with(prefix))
        {
            continue;
        }
        libraries.push((soname.to_owned(), PathBuf::from(path)));
    }
    Ok(libraries)
}

/// Sets an object's search path. `DT_RPATH` rather than `DT_RUNPATH`:
/// it also applies to the libraries it loads.
fn set_rpath(object: &Path, rpath: &str) -> Result {
    run(Command::new("patchelf")
        .args(["--force-rpath", "--set-rpath", rpath])
        .arg(object))
    .map_err(|err| format!("{err} (is patchelf installed?)"))
}

fn write_launcher(path: &Path, program: &str) -> Result {
    let script = format!(
        r#"#!/bin/sh
# Runs {program} with the libraries and data in this folder.
here="$(dirname "$(readlink -f "$0")")"
export XDG_DATA_DIRS="$here/share:${{XDG_DATA_DIRS:-/usr/local/share:/usr/share}}"
export GSETTINGS_SCHEMA_DIR="$here/share/glib-2.0/schemas"
# GTK's and GIO's modules come from this folder (none but the SVG loader),
# not from the system's GTK, which may be a different version.
export GTK_EXE_PREFIX="$here" GTK_DATA_PREFIX="$here"
export GIO_MODULE_DIR="$here/lib/gio/modules"
export GSETTINGS_BACKEND="${{GSETTINGS_BACKEND:-memory}}"
export GDK_PIXBUF_MODULEDIR="$here/lib/gdk-pixbuf-2.0/2.10.0/loaders"
cache="${{XDG_CACHE_HOME:-$HOME/.cache}}/nox"
export GDK_PIXBUF_MODULE_FILE="$cache/loaders-$(printf %s "$here" | cksum | cut -d' ' -f1).cache"
if [ ! -s "$GDK_PIXBUF_MODULE_FILE" ]; then
    mkdir -p "$cache"
    "$here/lib/nox/gdk-pixbuf-query-loaders" > "$GDK_PIXBUF_MODULE_FILE" 2>/dev/null
fi
exec "$here/bin/{program}" "$@"
"#
    );
    crate::write_executable(path, &script)
}

const README: &str = "\
Nox for Linux (x86-64)

Run ./nox to start the editor, or ./nox-cli for the command-line tool.
Everything Nox needs comes with it, except the graphics driver (with
Vulkan 1.3) and a desktop with GTK's usual base: glibc 2.41 or newer,
fontconfig, ALSA, X11 or Wayland.

To add Nox to your applications menu, copy
share/applications/io.github.feliwir.Nox.desktop to
~/.local/share/applications/ and change its Exec= line to the full path
of ./nox, and copy share/icons/hicolor to ~/.local/share/icons/hicolor.

Nox is free software under the GNU GPL, version 3 or later (LICENSE). It
includes GTK, FFmpeg (with x264 and x265) and other libraries; their
licenses are in share/doc and share/licenses.
";
