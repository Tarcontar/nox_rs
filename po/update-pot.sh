#!/bin/sh
# Regenerates po/nox.pot from the files in po/POTFILES and merges it into
# every catalog in po/LINGUAS. Needs GNU gettext (`apt install gettext`).
# The `.mo` files themselves are compiled by crates/nox-gui/build.rs.
set -eu
cd "$(dirname "$0")/.."

grep '\.rs$' po/POTFILES > po/.rust-files
grep '\.ui$' po/POTFILES > po/.ui-files
# Rust: gettext("…"), n_("…") in constant tables, pgettext("context", "…")
# and ngettext("one", "many", n). Placeholders are `{name}`, see i18n::fill.
xgettext --from-code=UTF-8 --language=C --no-wrap --sort-by-file \
    --keyword= --keyword=gettext --keyword=n_ --keyword=pgettext:1c,2 \
    --keyword=ngettext:1,2 --flag=gettext:1:python-brace-format \
    --package-name=nox --files-from=po/.rust-files --output=po/.rust.pot
xgettext --from-code=UTF-8 --language=Glade --no-wrap --sort-by-file \
    --package-name=nox --files-from=po/.ui-files --output=po/.ui.pot
msgcat --no-wrap --use-first po/.rust.pot po/.ui.pot --output-file=po/nox.pot
rm po/.rust-files po/.ui-files po/.rust.pot po/.ui.pot

for lang in $(grep -v '^#' po/LINGUAS); do
    msgmerge --quiet --update --backup=none --no-wrap "po/$lang.po" po/nox.pot
    msgfmt --check --statistics --output-file=/dev/null "po/$lang.po"
done
