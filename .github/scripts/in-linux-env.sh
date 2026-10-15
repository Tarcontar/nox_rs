#!/bin/sh
# Runs a command in the Linux build environment (nox-build-linux), in the
# checkout, as the runner's user. Crates download to .ci/cargo-home and
# builds go to target/debian13, both cached by the workflows.
set -eu
mkdir -p .ci/cargo-home
exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    --env HOME=/tmp \
    --env CARGO_HOME=/src/.ci/cargo-home \
    --env CARGO_TARGET_DIR=/src/target/debian13 \
    --env CARGO_TERM_COLOR=always \
    --env NOX_REQUIRE_GPU \
    --volume "$PWD:/src" --workdir /src \
    nox-build-linux "$@"
