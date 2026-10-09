//! Translations, with gettext (which GtkBuilder uses for `.ui` files).
//!
//! Strings in Rust go through `gettext` / `ngettext` / `pgettext`; strings
//! in `.ui` files are marked `translatable="yes"`. `po/update-pot.sh`
//! extracts both into `po/nox.pot`; `build.rs` compiles `po/*.po`.
//! Messages with values use named placeholders, so translators see whole
//! sentences and can reorder them: `fill(&gettext("Cannot open {path}"), …)`.

use gettextrs::{LocaleCategory, bind_textdomain_codeset, bindtextdomain, setlocale, textdomain};
use std::fmt::Display;
use std::path::PathBuf;

pub use gettextrs::{gettext, ngettext, pgettext};

/// The gettext domain of Nox's catalogs (`nox.mo`).
pub const DOMAIN: &str = "nox";

/// Selects the user's language and where catalogs are. Call before any
/// UI is built.
pub fn init() {
    // SAFETY: called at startup, before any other thread exists that could
    // read the locale.
    unsafe {
        setlocale(LocaleCategory::LcAll, "");
    }
    let dir = locale_dir();
    let result = bindtextdomain(DOMAIN, dir.clone())
        .and_then(|_| bind_textdomain_codeset(DOMAIN, "UTF-8"))
        .and_then(|_| textdomain(DOMAIN));
    match result {
        Ok(_) => tracing::debug!("translations from {}", dir.display()),
        Err(err) => tracing::warn!("translations unavailable: {err}"),
    }
}

/// `NOX_LOCALEDIR` if set; else `share/locale` next to an installed
/// binary (`bin/nox`); else the catalogs `build.rs` compiled, for
/// development builds.
fn locale_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NOX_LOCALEDIR") {
        return dir.into();
    }
    let installed = std::env::current_exe().ok().and_then(|exe| {
        let bin = exe.parent()?;
        (bin.file_name()? == "bin")
            .then(|| bin.parent().map(|prefix| prefix.join("share/locale")))?
    });
    installed.unwrap_or_else(|| PathBuf::from(env!("NOX_BUILD_LOCALEDIR")))
}

/// Marks a string in a constant table for extraction (`xgettext
/// --keyword=n_`); translate it with `gettext` where it is shown.
pub const fn n_(text: &'static str) -> &'static str {
    text
}

/// The translated description of a scaling mode (as in Resolve).
pub fn scaling_mode_name(mode: nox_project::ScalingMode) -> String {
    use nox_project::ScalingMode;
    match mode {
        ScalingMode::Fit => gettext("Scale entire image to fit"),
        ScalingMode::Fill => gettext("Scale full frame with crop"),
        ScalingMode::CenterCrop => gettext("Center crop with no resizing"),
        ScalingMode::Stretch => gettext("Stretch frame to all corners"),
    }
}

/// The translated name of a blend mode (as in the clip menu).
pub fn blend_mode_name(mode: nox_project::BlendMode) -> String {
    use nox_project::BlendMode;
    match mode {
        BlendMode::Normal => pgettext("blend mode", "Normal"),
        BlendMode::Add => pgettext("blend mode", "Add"),
        BlendMode::Multiply => pgettext("blend mode", "Multiply"),
        BlendMode::Screen => pgettext("blend mode", "Screen"),
    }
}

/// Replaces each `{name}` in `template` (a translated message) with its
/// value.
pub fn fill(template: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut text = template.to_owned();
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), &value.to_string());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_in_any_order() {
        let text = fill("{b} then {a}, {a} again", &[("a", &1), ("b", &"two")]);
        assert_eq!(text, "two then 1, 1 again");
    }
}
