use std::path::{Path, PathBuf};

/// Data directory: `./data` next to the executable (or the `.app` bundle) when a
/// `portable` marker file sits there, otherwise `<OS config dir>/MoliTerm`.
pub fn data_dir() -> PathBuf {
    let portable = std::env::current_exe()
        .ok()
        .and_then(|exe| portable_base(&exe))
        .filter(|base| base.join("portable").is_file());
    match portable {
        Some(base) => base.join("data"),
        None => directories::BaseDirs::new()
            .map(|d| d.config_dir().join("MoliTerm"))
            .unwrap_or_else(|| PathBuf::from("data")),
    }
}

/// Folder that holds the executable, or the folder holding `X.app` on macOS.
fn portable_base(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let bundle = dir.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"));
    Some(bundle.and_then(Path::parent).unwrap_or(dir).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_base_is_outside_app() {
        let exe = Path::new("/Apps/MoliTerm.app/Contents/MacOS/moliterm");
        assert_eq!(portable_base(exe), Some(PathBuf::from("/Apps")));
        let exe = Path::new("/opt/moli/moliterm");
        assert_eq!(portable_base(exe), Some(PathBuf::from("/opt/moli")));
    }
}
