//! Windows: embed the app icon into the .exe with `windres` (mingw) or `rc` (MSVC).

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/MoliTerm.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let (Ok(out), Ok(dir)) = (std::env::var("OUT_DIR"), std::env::var("CARGO_MANIFEST_DIR")) else {
        return;
    };
    let ico = std::path::Path::new(&dir).join("../../assets/icons/MoliTerm.ico");
    let rc = std::path::Path::new(&out).join("app.rc");
    // `\\` → `/`: both resource compilers accept forward slashes.
    let ico_path = ico.display().to_string().replace('\\', "/");
    if std::fs::write(&rc, format!("1 ICON \"{ico_path}\"\n")).is_err() {
        return;
    }
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let res = std::path::Path::new(&out).join(if msvc { "app.res" } else { "app.o" });
    let ok = if msvc {
        std::process::Command::new("rc")
            .arg("/nologo")
            .arg(format!("/fo{}", res.display()))
            .arg(&rc)
            .status()
    } else {
        let windres = if cfg!(windows) {
            "windres"
        } else {
            "x86_64-w64-mingw32-windres"
        };
        std::process::Command::new(windres)
            .args(["-O", "coff", "-i"])
            .arg(&rc)
            .arg("-o")
            .arg(&res)
            .status()
    };
    match ok {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", res.display()),
        // ponytail: no resource compiler → exe without icon, build still succeeds.
        _ => println!("cargo:warning=resource compiler not found; MoliTerm.exe gets no icon"),
    }
}
