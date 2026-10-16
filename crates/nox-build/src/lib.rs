//! Build-script helpers shared by the crates with GPU shaders: compiles
//! GLSL compute shaders to SPIR-V with glslc or glslangValidator, found on
//! PATH or in `$VULKAN_SDK/bin`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Compiles every `*.comp` in `dir` to `OUT_DIR/<name>.comp.spv`, and each
/// variant (source, output name, preprocessor defines) to
/// `OUT_DIR/<name>.comp.spv`. Includes resolve in `dir` and in `OUT_DIR`
/// (for generated headers). Panics with a message on failure, as build
/// scripts do.
pub fn compile_shaders(dir: &Path, variants: &[(&str, &str, &[&str])]) {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed={}", dir.display());
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    let compiler = find_compiler().unwrap_or_else(|| {
        panic!(
            "no GLSL compiler found: install the Vulkan SDK or glslang (glslc or glslangValidator)"
        )
    });
    for entry in std::fs::read_dir(dir).expect("shader folder") {
        let path = entry.expect("shader file").path();
        if path.extension().is_some_and(|e| e == "comp") {
            let out = out_dir
                .join(path.file_name().expect("name"))
                .with_extension("comp.spv");
            compile(&compiler, &path, &out, &out_dir, &[]);
        }
    }
    for (source, name, defines) in variants {
        let out = out_dir.join(format!("{name}.comp.spv"));
        compile(&compiler, &dir.join(source), &out, &out_dir, defines);
    }
}

/// Whether the GLSL compiler accepts all of `extensions` (e.g.
/// `GL_NV_cooperative_matrix2`, which older glslang lacks), by compiling an
/// empty compute shader that requires them in `OUT_DIR`.
pub fn compiler_supports(extensions: &[&str]) -> bool {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let Some(compiler) = find_compiler() else {
        return false;
    };
    let requires: String = extensions
        .iter()
        .map(|extension| format!("#extension {extension} : require\n"))
        .collect();
    let source =
        format!("#version 450\n{requires}layout(local_size_x = 1) in;\nvoid main() {{}}\n");
    let probe = out_dir.join("extension_probe.comp");
    std::fs::write(&probe, source).expect("write extension probe");
    command(
        &compiler,
        &probe,
        &out_dir.join("extension_probe.comp.spv"),
        &out_dir,
        &[],
    )
    .output()
    .is_ok_and(|o| o.status.success())
}

enum Compiler {
    Glslc(PathBuf),
    GlslangValidator(PathBuf),
}

fn find_compiler() -> Option<Compiler> {
    let sdk_bin = std::env::var_os("VULKAN_SDK").map(|sdk| PathBuf::from(sdk).join("bin"));
    let candidates = |name: &str| {
        let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        sdk_bin
            .iter()
            .map(move |dir| dir.join(&exe))
            .chain(std::iter::once(PathBuf::from(name)))
    };
    let works = |path: &Path| {
        Command::new(path)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    candidates("glslc")
        .find(|p| works(p))
        .map(Compiler::Glslc)
        .or_else(|| {
            candidates("glslangValidator")
                .find(|p| works(p))
                .map(Compiler::GlslangValidator)
        })
}

fn command(
    compiler: &Compiler,
    input: &Path,
    output: &Path,
    include: &Path,
    defines: &[&str],
) -> Command {
    let mut cmd = match compiler {
        Compiler::Glslc(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(["--target-env=vulkan1.3", "-O"]);
            cmd
        }
        Compiler::GlslangValidator(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(["-V", "--target-env", "vulkan1.3"]);
            cmd
        }
    };
    cmd.arg(format!("-I{}", include.display()));
    for define in defines {
        cmd.arg(format!("-D{define}"));
    }
    cmd.arg("-o").arg(output).arg(input);
    cmd
}

fn compile(compiler: &Compiler, input: &Path, output: &Path, include: &Path, defines: &[&str]) {
    let result = command(compiler, input, output, include, defines)
        .output()
        .expect("failed to run GLSL compiler");
    if !result.status.success() {
        panic!(
            "compiling {} failed:\n{}{}",
            input.display(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
