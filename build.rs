use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=ghost.rc");
    println!("cargo:rerun-if-changed=ghost.ico");
    println!("cargo:rerun-if-changed=ghost.manifest");
    println!("cargo:rerun-if-env-changed=RC");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let target = env::var("TARGET").unwrap_or_default();
    let arch = if target.starts_with("aarch64-") {
        "arm64"
    } else if target.starts_with("i686-") {
        "x86"
    } else {
        "x64"
    };
    let rc = find_resource_compiler(arch)
        .expect("No se encontró rc.exe del Windows SDK para incrustar ghost.ico");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR no definido"));
    let resource = out_dir.join("ghost.res");
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR no definido"));

    let status = Command::new(rc)
        .current_dir(manifest_dir)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg("ghost.rc")
        .status()
        .expect("No se pudo ejecutar rc.exe");
    assert!(status.success(), "Falló la compilación del icono de Ghost");

    println!("cargo:rustc-link-arg-bin=ghost={}", resource.display());
}

fn find_resource_compiler(arch: &str) -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        let path = PathBuf::from(rc);
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(output) = Command::new("where.exe").arg("rc.exe").output() {
        if output.status.success() {
            if let Some(path) = String::from_utf8_lossy(&output.stdout).lines().next() {
                return Some(PathBuf::from(path.trim()));
            }
        }
    }

    let kits = PathBuf::from(env::var_os("ProgramFiles(x86)")?)
        .join("Windows Kits")
        .join("10")
        .join("bin");
    let mut versions: Vec<_> = fs::read_dir(kits).ok()?.filter_map(Result::ok).collect();
    versions.sort_by_key(|entry| entry.file_name());
    versions.reverse();

    versions
        .into_iter()
        .map(|version| version.path().join(arch).join("rc.exe"))
        .find(|path| path.exists())
}
