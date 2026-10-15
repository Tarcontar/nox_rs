//! Compiles the kernels (`shaders/*.comp`) to SPIR-V.

fn main() {
    nox_build::compile_shaders(std::path::Path::new("shaders"), &[]);
}
