//! Embeds the application icon and version metadata into the Windows executable.
//!
//! A Windows executable carries its Explorer/taskbar icon in a resource section, which must be
//! compiled to a `.res` and handed to the linker. We shell out to the Windows SDK `rc.exe`
//! directly rather than pulling in a build-dependency; the script is a no-op on non-MSVC targets
//! so the crate still builds elsewhere.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Locate `rc.exe`: an explicit `RC` override, then `PATH`, then the newest Windows SDK under
/// `Windows Kits\10\bin\<version>\x64`.
fn find_rc() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("RC") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let cand = dir.join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let kits_root = env::var_os("ProgramFiles(x86)")
        .or_else(|| env::var_os("ProgramFiles"))
        .map(PathBuf::from)?
        .join("Windows Kits")
        .join("10")
        .join("bin");
    let mut newest: Option<(Vec<u64>, PathBuf)> = None;
    for entry in std::fs::read_dir(kits_root).ok()?.flatten() {
        let rc = entry.path().join("x64").join("rc.exe");
        if rc.is_file() {
            let ver: Vec<u64> = entry
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|s| s.parse().ok())
                .collect();
            if newest.as_ref().is_none_or(|(best, _)| ver > *best) {
                newest = Some((ver, rc));
            }
        }
    }
    newest.map(|(_, p)| p)
}

fn version_resource() -> String {
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let quad = {
        let mut parts: Vec<u32> = version
            .split(['.', '-'])
            .take(3)
            .filter_map(|s| s.parse().ok())
            .collect();
        while parts.len() < 4 {
            parts.push(0);
        }
        format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3])
    };
    format!(
        r#"1 VERSIONINFO
 FILEVERSION {quad}
 PRODUCTVERSION {quad}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0x0L
 FILEOS 0x40004L
 FILETYPE 0x1L
 FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "NeoMonitor"
            VALUE "FileDescription", "NeoMonitor - hardware and performance monitor"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "neomonitor"
            VALUE "OriginalFilename", "neomonitor.exe"
            VALUE "ProductName", "NeoMonitor"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let icon = manifest.join("assets").join("neomonitor.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=icon resource is only embedded for the MSVC toolchain");
        return;
    }

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; building without an application icon");
        return;
    };
    if !icon.is_file() {
        println!(
            "cargo:warning=icon not found at {}; run assets/gen_icon.ps1",
            icon.display()
        );
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    let rc_file = out.join("neomonitor.rc");
    let res_file = out.join("neomonitor.res");

    // rc.exe treats backslash as an escape inside string literals, so a Windows path must be
    // escaped (or written with forward slashes) before it goes into the resource script.
    let icon_escaped = icon.to_string_lossy().replace('\\', "\\\\");
    let script = format!("1 ICON \"{icon_escaped}\"\n{}\n", version_resource());
    if std::fs::write(&rc_file, script).is_err() {
        println!("cargo:warning=failed to write resource script");
        return;
    }

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", res_file.display());
            println!("cargo:rerun-if-changed={}", res_file.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe exited with {s}; building without an icon"),
        Err(e) => println!("cargo:warning=failed to run rc.exe ({e}); building without an icon"),
    }
}
