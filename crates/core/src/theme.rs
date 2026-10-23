use serde::Deserialize;
use std::path::Path;

/// `#rrggbb` color.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(try_from = "String")]
pub struct Rgb(pub u8, pub u8, pub u8);

impl TryFrom<String> for Rgb {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let hex = s.strip_prefix('#').unwrap_or(&s);
        let n = u32::from_str_radix(hex, 16).map_err(|e| format!("{s}: {e}"))?;
        if hex.len() != 6 {
            return Err(format!("{s}: expected #rrggbb"));
        }
        Ok(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct UiColors {
    pub bg: Rgb,
    pub panel: Rgb,
    pub surface: Rgb,
    pub border: Rgb,
    pub fg: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub on_accent: Rgb,
    pub selection: Rgb,
    pub danger: Rgb,
    pub warning: Rgb,
    pub success: Rgb,
}

#[derive(Deserialize, Clone, Debug)]
pub struct TermColors {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub ansi: [Rgb; 16],
}

#[derive(Deserialize, Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub ui: UiColors,
    pub term: TermColors,
}

const BUILTIN: &[&str] = &[
    include_str!("../../../assets/themes/ayu-dark.toml"),
    include_str!("../../../assets/themes/ayu-mirage.toml"),
    include_str!("../../../assets/themes/ayu-light.toml"),
    include_str!("../../../assets/themes/monokai.toml"),
];

/// Built-in themes followed by valid user themes from `user_dir/*.toml`.
/// A user theme with a built-in's name replaces it. Broken files are skipped.
pub fn load_all(user_dir: &Path) -> Vec<Theme> {
    let mut themes: Vec<Theme> = BUILTIN.iter().filter_map(|s| toml::from_str(s).ok()).collect();
    let Ok(entries) = std::fs::read_dir(user_dir) else {
        return themes;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<Theme>(&s).map_err(|e| e.to_string()));
        match parsed {
            Ok(t) => match themes.iter_mut().find(|x| x.name == t.name) {
                Some(slot) => *slot = t,
                None => themes.push(t),
            },
            Err(e) => tracing::warn!("skipping theme {}: {e}", path.display()),
        }
    }
    themes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        for s in BUILTIN {
            toml::from_str::<Theme>(s).unwrap();
        }
        let names: Vec<_> = load_all(Path::new("/nonexistent"))
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["Ayu Dark", "Ayu Mirage", "Ayu Light", "Monokai"]);
    }

    #[test]
    fn hex() {
        assert_eq!(Rgb::try_from("#0a0B0c".to_string()), Ok(Rgb(10, 11, 12)));
        assert!(Rgb::try_from("#fff".to_string()).is_err());
        assert!(Rgb::try_from("#gggggg".to_string()).is_err());
    }
}
