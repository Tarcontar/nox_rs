//! Compiles the GResource bundle (`resources/`: `.ui` templates, menus) and
//! the translations (`po/*.po` at the workspace root) into `.mo` catalogs
//! in `OUT_DIR/locale`, so building Nox needs no gettext tools.

use std::path::{Path, PathBuf};

#[path = "po.rs"]
mod po;

fn main() {
    // `xml-stripblanks` needs xmllint (libxml2-utils); without it the .ui
    // files are embedded as they are, which works but is larger.
    let exe = format!("xmllint{}", std::env::consts::EXE_SUFFIX);
    let xmllint = std::env::var_os("XMLLINT").is_some()
        || std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(&exe).is_file()));
    if !xmllint {
        println!(
            "cargo:warning=xmllint not found (install libxml2-utils); .ui files are not stripped"
        );
    }
    glib_build_tools::compile_resources(
        &["resources"],
        "resources/nox.gresource.xml",
        "nox.gresource",
    );

    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("locale");
    println!("cargo:rerun-if-changed=../../po");
    po::compile_catalogs(Path::new("../../po"), &out).unwrap_or_else(|err| panic!("{err}"));
    println!("cargo:rustc-env=NOX_BUILD_LOCALEDIR={}", out.display());

    // nox.exe's icon and version information.
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../../data/icons/windows/io.github.feliwir.Nox.ico")
            .set("FileDescription", "Nox")
            .set("ProductName", "Nox");
        resource
            .compile()
            .unwrap_or_else(|err| panic!("Windows resources: {err}"));
    }
}
