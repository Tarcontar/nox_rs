//! Compiles the GResource bundle (`resources/`: `.ui` templates, menus) and
//! the translations (`po/*.po` at the workspace root) into `.mo` catalogs
//! in `OUT_DIR/locale`, so building Nox needs no gettext tools.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    // `xml-stripblanks` needs xmllint (libxml2-utils); without it the .ui
    // files are embedded as they are, which works but is larger.
    let xmllint = std::env::var_os("XMLLINT").is_some()
        || std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("xmllint").is_file()));
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
    let po_dir = Path::new("../../po");
    println!("cargo:rerun-if-changed=../../po");
    let linguas = std::fs::read_to_string(po_dir.join("LINGUAS")).unwrap_or_default();
    for lang in linguas.split_whitespace().filter(|l| !l.starts_with('#')) {
        let po = po_dir.join(format!("{lang}.po"));
        let text =
            std::fs::read_to_string(&po).unwrap_or_else(|err| panic!("{}: {err}", po.display()));
        let catalog = parse_po(&text).unwrap_or_else(|err| panic!("{}: {err}", po.display()));
        let dir = out.join(lang).join("LC_MESSAGES");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nox.mo"), write_mo(&catalog)).unwrap();
    }
    println!("cargo:rustc-env=NOX_BUILD_LOCALEDIR={}", out.display());
}

/// Original (with context and plural joined as gettext stores them) to
/// translation (plural forms joined by NUL).
type Catalog = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
struct Entry {
    context: Option<String>,
    id: Option<String>,
    plural: Option<String>,
    strings: BTreeMap<usize, String>,
    fuzzy: bool,
}

/// Parses a `.po` file (entries separated by blank lines, as gettext
/// writes them); untranslated and fuzzy entries are left out. The header
/// entry (msgid ""), with its Plural-Forms, is kept.
fn parse_po(text: &str) -> Result<Catalog, String> {
    let mut catalog = Catalog::new();
    let text = text.replace("\r\n", "\n");
    for (index, block) in text.split("\n\n").enumerate() {
        let entry = parse_entry(block).map_err(|e| format!("entry {}: {e}", index + 1))?;
        let Some(id) = entry.id else { continue };
        if entry.fuzzy && !id.is_empty() {
            continue;
        }
        let translations: Vec<&String> = entry.strings.values().collect();
        if translations.iter().all(|t| t.is_empty()) {
            continue;
        }
        let mut original = Vec::new();
        if let Some(context) = &entry.context {
            original.extend_from_slice(context.as_bytes());
            original.push(4);
        }
        original.extend_from_slice(id.as_bytes());
        if let Some(plural) = &entry.plural {
            original.push(0);
            original.extend_from_slice(plural.as_bytes());
        }
        let translation = translations
            .iter()
            .map(|t| t.as_bytes())
            .collect::<Vec<_>>()
            .join(&0u8);
        catalog.insert(original, translation);
    }
    Ok(catalog)
}

fn parse_entry(block: &str) -> Result<Entry, String> {
    let mut entry = Entry::default();
    // The field continuation lines ("...") append to.
    let mut field: Option<&mut String> = None;
    for line in block.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(flags) = line.strip_prefix("#,") {
            entry.fuzzy |= flags.split(',').any(|f| f.trim() == "fuzzy");
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with('"') {
            let value = unquote(line).ok_or("bad string")?;
            field
                .as_mut()
                .ok_or("continuation without a field")?
                .push_str(&value);
            continue;
        }
        let (keyword, rest) = line
            .split_once(' ')
            .ok_or("expected a keyword and a string")?;
        let value = unquote(rest.trim()).ok_or("expected a quoted string")?;
        let slot = match keyword {
            "msgctxt" => entry.context.insert(value),
            "msgid" => entry.id.insert(value),
            "msgid_plural" => entry.plural.insert(value),
            "msgstr" => entry.strings.entry(0).or_insert(value),
            k if k.starts_with("msgstr[") && k.ends_with(']') => {
                let n: usize = k["msgstr[".len()..k.len() - 1]
                    .parse()
                    .map_err(|_| "bad plural index")?;
                entry.strings.entry(n).or_insert(value)
            }
            other => return Err(format!("unknown keyword {other}")),
        };
        field = Some(slot);
    }
    Ok(entry)
}

/// Decodes a C-style quoted string.
fn unquote(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

/// Writes a GNU `.mo` catalog (little endian, no hash table).
fn write_mo(catalog: &Catalog) -> Vec<u8> {
    let count = catalog.len() as u32;
    let header = 28u32;
    let originals_table = header;
    let translations_table = originals_table + 8 * count;
    let data_offset = translations_table + 8 * count;
    let mut tables = Vec::new();
    let mut data = Vec::new();
    let place = |bytes: &[u8], data: &mut Vec<u8>| {
        let entry = (bytes.len() as u32, data_offset + data.len() as u32);
        data.extend_from_slice(bytes);
        data.push(0);
        entry
    };
    let originals: Vec<_> = catalog.keys().map(|k| place(k, &mut data)).collect();
    let translations: Vec<_> = catalog.values().map(|v| place(v, &mut data)).collect();
    for (length, offset) in originals.iter().chain(&translations) {
        tables.extend_from_slice(&length.to_le_bytes());
        tables.extend_from_slice(&offset.to_le_bytes());
    }
    let mut mo = Vec::new();
    for word in [
        0x9504_12de_u32,
        0,
        count,
        originals_table,
        translations_table,
        0,
        header,
    ] {
        mo.extend_from_slice(&word.to_le_bytes());
    }
    mo.extend_from_slice(&tables);
    mo.extend_from_slice(&data);
    mo
}
