//! Checks that user-visible text is translatable and that the translation
//! template is up to date. Text counts as words when it has a run of three
//! or more letters ("dB", "100%" and timecodes don't).
//!
//! A line that intentionally shows a fixed string (a format name, say)
//! carries a `// no-i18n` comment.

use std::path::{Path, PathBuf};

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn files(dir: &str, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(crate_dir().join(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .collect();
    files.sort();
    files
}

fn has_words(text: &str) -> bool {
    let mut run = 0;
    for c in text.chars() {
        run = if c.is_alphabetic() { run + 1 } else { 0 };
        if run >= 3 {
            return true;
        }
    }
    false
}

/// Calls whose string argument is shown to the user.
const RUST_SETTERS: &[&str] = &[
    ".label(\"",
    ".set_label(\"",
    "with_label(\"",
    ".tooltip_text(\"",
    ".set_tooltip_text(Some(\"",
    ".title(\"",
    ".set_title(Some(\"",
    ".text(\"",
    ".set_text(\"",
    ".set_markup(\"",
    ".message(\"",
    ".detail(\"",
    ".buttons([\"",
    ".set_name(Some(\"",
    ".edit(\"",
    "edit_selected_clip(\"",
    "show_error(\"",
    "set_failed(\"",
    "set_summary(\"",
    ".property(\"summary\", \"",
    "Timeline::new(\"",
];

#[test]
fn rust_strings_are_translatable() {
    let mut problems = Vec::new();
    for path in files("src", "rs") {
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let allowed = line.contains("// no-i18n")
                || index > 0 && lines[index - 1].trim_start().starts_with("// no-i18n");
            if allowed || line.trim_start().starts_with("//") {
                continue;
            }
            for setter in RUST_SETTERS {
                let Some(start) = line.find(setter) else {
                    continue;
                };
                let literal = &line[start + setter.len()..];
                let literal = &literal[..literal.find('"').unwrap_or(literal.len())];
                if has_words(literal) {
                    problems.push(format!(
                        "{}:{}: \"{literal}\" is not translatable; wrap it in gettext()",
                        path.display(),
                        index + 1
                    ));
                }
            }
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

#[test]
fn ui_strings_are_translatable() {
    let mut problems = Vec::new();
    for path in files("resources/ui", "ui") {
        let text = std::fs::read_to_string(&path).unwrap();
        for (index, line) in text.lines().enumerate() {
            for name in ["label", "title", "tooltip-text", "text", "placeholder-text"] {
                for tag in ["property", "attribute"] {
                    let open = format!("<{tag} name=\"{name}\"");
                    let Some(start) = line.find(&open) else {
                        continue;
                    };
                    let rest = &line[start..];
                    let Some(end) = rest.find('>') else { continue };
                    let value = &rest[end + 1..];
                    let value = &value[..value.find('<').unwrap_or(value.len())];
                    if has_words(value) && !rest[..end].contains("translatable=\"yes\"") {
                        problems.push(format!(
                            "{}:{}: \"{value}\" needs translatable=\"yes\"",
                            path.display(),
                            index + 1
                        ));
                    }
                }
            }
        }
        if !text.contains("<interface domain=\"nox\">") {
            problems.push(format!(
                "{}: <interface> needs domain=\"nox\"",
                path.display()
            ));
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

/// Every file with translatable text is listed in po/POTFILES, and every
/// string marked in Rust is in po/nox.pot (run po/update-pot.sh otherwise).
#[test]
fn template_is_up_to_date() {
    let root = crate_dir().join("../..");
    let potfiles = std::fs::read_to_string(root.join("po/POTFILES")).unwrap();
    let pot = std::fs::read_to_string(root.join("po/nox.pot")).unwrap();
    let listed: Vec<&str> = potfiles.lines().filter(|l| !l.starts_with('#')).collect();
    for file in &listed {
        assert!(root.join(file).exists(), "po/POTFILES lists missing {file}");
    }
    let mut problems = Vec::new();
    let sources = files("src", "rs")
        .into_iter()
        .chain(files("resources/ui", "ui"));
    for path in sources {
        let text = std::fs::read_to_string(&path).unwrap();
        let relative = format!(
            "crates/nox-gui/{}",
            path.strip_prefix(crate_dir()).unwrap().display()
        );
        let marks = ["gettext(\"", "n_(\"", "translatable=\"yes\""];
        if marks.iter().any(|m| text.contains(m)) && !listed.contains(&relative.as_str()) {
            problems.push(format!("{relative} is missing from po/POTFILES"));
        }
        // The first argument of each Rust marker; `pgettext` and `ngettext`
        // start with a context or the singular, also msgctxt/msgid lines.
        for mark in ["gettext(\"", "n_(\""] {
            for (start, _) in text.match_indices(mark) {
                let literal = &text[start + mark.len()..];
                let literal = &literal[..literal.find('"').unwrap_or(0)];
                let entry = format!("\"{}\"", literal.replace('\\', "\\\\"));
                let in_pot = pot.lines().any(|l| {
                    (l.starts_with("msgid ") || l.starts_with("msgctxt ")) && l.ends_with(&entry)
                });
                if !in_pot {
                    problems.push(format!("\"{literal}\" ({relative}) is not in po/nox.pot"));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "\n{}\nRun po/update-pot.sh.\n",
        problems.join("\n")
    );
}
