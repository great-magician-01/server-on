fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=ui/tray.slint");
    println!("cargo:rerun-if-changed=../../assets/icon/server-on.ico");
    println!("cargo:rerun-if-changed=../../assets/icon/server-on.svg");
    println!("cargo:rerun-if-changed=../../assets/icon/server-on-256.png");
    println!("cargo:rerun-if-changed=../../assets/icon/tray-32.png");
    println!("cargo:rerun-if-env-changed=SLINT_STYLE");

    let style = std::env::var("SLINT_STYLE").unwrap_or_else(|_| "fluent-dark".into());
    let config = slint_build::CompilerConfiguration::new().with_style(style);
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile crates/gui/ui/app.slint");

    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
fn embed_windows_icon() {
    use std::{fs, path::PathBuf, process::Command};

    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let icon = manifest.join("../../assets/icon/server-on.ico");
    let rc_file = out_dir.join("server-on.rc");
    let resource_file = out_dir.join("server-on.res");
    let rc_icon = icon.to_string_lossy().replace('\\', "/");
    let rc = format!(
        "#include <winver.h>\n1 ICON \"{}\"\n1 VERSIONINFO\nFILEVERSION 0,1,0,0\nPRODUCTVERSION 0,1,0,0\nFILEFLAGSMASK 0x3fL\nFILEFLAGS 0x0L\nFILEOS 0x40004L\nFILETYPE 0x1L\nFILESUBTYPE 0x0L\nBEGIN\n  BLOCK \"StringFileInfo\"\n  BEGIN\n    BLOCK \"040904b0\"\n    BEGIN\n      VALUE \"CompanyName\", \"ServerOn\"\n      VALUE \"FileDescription\", \"ServerOn Local Service Manager\"\n      VALUE \"FileVersion\", \"0.1.0\"\n      VALUE \"InternalName\", \"server-on\"\n      VALUE \"OriginalFilename\", \"server-on.exe\"\n      VALUE \"ProductName\", \"ServerOn\"\n      VALUE \"ProductVersion\", \"0.1.0\"\n    END\n  END\n  BLOCK \"VarFileInfo\"\n  BEGIN\n    VALUE \"Translation\", 0x409, 1200\n  END\nEND\n",
        rc_icon
    );
    fs::write(&rc_file, rc).expect("failed to write Windows resource script");
    let rc_exe =
        find_rc_exe().expect("Windows SDK rc.exe was not found; install the SDK or set RC");
    let mut command = Command::new(rc_exe);
    command.arg("/nologo");
    for directory in resource_include_dirs() {
        command.arg("/i").arg(directory);
    }
    let status = command
        .arg("/fo")
        .arg(&resource_file)
        .arg(&rc_file)
        .status()
        .expect("failed to execute Windows resource compiler");
    assert!(
        status.success(),
        "Windows resource compilation failed: {status}"
    );
    println!(
        "cargo:rustc-link-arg-bin=server-on={}",
        resource_file.display()
    );
}

#[cfg(windows)]
fn find_rc_exe() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("RC").map(std::path::PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    if let Some(path) = std::env::var_os("RC_EXE").map(std::path::PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    for root in [
        std::path::PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin"),
        std::path::PathBuf::from(r"C:\Program Files\Windows Kits\10\bin"),
    ] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut versions: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        versions.sort();
        versions.reverse();
        for version in versions {
            let candidate = version.join("x64").join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn resource_include_dirs() -> Vec<std::path::PathBuf> {
    for root in [
        std::path::PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\Include"),
        std::path::PathBuf::from(r"C:\Program Files\Windows Kits\10\Include"),
    ] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut versions: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        versions.sort();
        versions.reverse();
        for version in versions {
            if version.join("um").join("winver.h").is_file() {
                return vec![
                    version.join("um"),
                    version.join("shared"),
                    version.join("ucrt"),
                ];
            }
        }
    }
    Vec::new()
}
