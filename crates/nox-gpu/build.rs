//! Compiles `shaders/*.comp` (GLSL) to SPIR-V in OUT_DIR with glslc or
//! glslangValidator, from PATH or `$VULKAN_SDK/bin`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");

    let compiler = find_compiler().unwrap_or_else(|| {
        panic!(
            "no GLSL compiler found: install the Vulkan SDK or glslang (glslc or glslangValidator)"
        )
    });

    for entry in std::fs::read_dir("shaders").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "comp") {
            let out = out_dir
                .join(path.file_name().unwrap())
                .with_extension("comp.spv");
            compile(&compiler, &path, &out);
        }
    }
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

fn compile(compiler: &Compiler, input: &Path, output: &Path) {
    let mut cmd = match compiler {
        Compiler::Glslc(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(["--target-env=vulkan1.3", "-O", "-o"])
                .arg(output)
                .arg(input);
            cmd
        }
        Compiler::GlslangValidator(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(["-V", "--target-env", "vulkan1.3", "-o"])
                .arg(output)
                .arg(input);
            cmd
        }
    };
    let result = cmd.output().expect("failed to run GLSL compiler");
    if !result.status.success() {
        panic!(
            "compiling {} failed:\n{}{}",
            input.display(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
