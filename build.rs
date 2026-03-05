fn main() {
    glib_build_tools::compile_resources(
        &["resources"],
        "resources/nox.gresource.xml",
        "nox.gresource",
    );
}
