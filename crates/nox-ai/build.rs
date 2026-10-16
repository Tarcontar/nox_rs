//! Compiles the kernels (`shaders/*.comp`) to SPIR-V, each also as
//! `<name>_f16` with F16 defined (16-bit tensors, `precision.glsl`), and
//! the matrix multiplications as `<name>_tensor` for tensor cores (F16
//! and TENSOR, `gemm_tensor.glsl`; convolutions also with tiles of 32×128
//! and 16×128), and convolutions with stride 1 as `conv_gemm_cm2` for
//! NVIDIA's cooperative matrices 2 (F16 and CM2; also with 64-row tiles).
//! Those need a glslang with `GL_NV_cooperative_matrix2` (older Vulkan
//! SDKs lack it): without one they're skipped, `nox_cm2` stays unset and
//! the convolutions run on `gemm_tensor.glsl` instead.

fn main() {
    let dir = std::path::Path::new("shaders");
    let names: Vec<(String, String)> = std::fs::read_dir(dir)
        .expect("shader folder")
        .filter_map(|entry| {
            let path = entry.expect("shader file").path();
            let file = path.file_name()?.to_str()?.to_owned();
            let stem = file.strip_suffix(".comp")?.to_owned();
            Some((file, format!("{stem}_f16")))
        })
        .collect();
    let mut variants: Vec<(&str, &str, &[&str])> = names
        .iter()
        .map(|(file, name)| (file.as_str(), name.as_str(), &["F16"][..]))
        .collect();
    variants.extend([
        ("conv_gemm.comp", "conv_gemm_tensor", &["F16", "TENSOR"][..]),
        (
            "conv_gemm.comp",
            "conv_gemm_tensor_32x128",
            &[
                "F16",
                "TENSOR",
                "TILE_M=32",
                "TILE_N=128",
                "SUBGROUP_ROWS=1",
            ][..],
        ),
        (
            "conv_gemm.comp",
            "conv_gemm_tensor_16x128",
            &[
                "F16",
                "TENSOR",
                "TILE_M=16",
                "TILE_N=128",
                "SUBGROUP_ROWS=1",
            ][..],
        ),
        ("matmul.comp", "matmul_tensor", &["F16", "TENSOR"][..]),
    ]);
    println!("cargo::rustc-check-cfg=cfg(nox_cm2)");
    if nox_build::compiler_supports(&["GL_KHR_cooperative_matrix", "GL_NV_cooperative_matrix2"]) {
        println!("cargo::rustc-cfg=nox_cm2");
        variants.extend([
            ("conv_gemm.comp", "conv_gemm_cm2", &["F16", "CM2"][..]),
            (
                "conv_gemm.comp",
                "conv_gemm_cm2_64",
                &["F16", "CM2", "TILE_M=64"][..],
            ),
        ]);
    } else {
        println!(
            "cargo::warning=glslang lacks GL_NV_cooperative_matrix2, skipping the CM2 kernels (update the Vulkan SDK)"
        );
    }
    nox_build::compile_shaders(dir, &variants);
}
