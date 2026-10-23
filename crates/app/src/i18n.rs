use fluent_bundle::{FluentArgs, FluentBundle, FluentResource, FluentValue};

const EN: &str = include_str!("../../../assets/i18n/en/main.ftl");
const RU: &str = include_str!("../../../assets/i18n/ru/main.ftl");

/// Fluent strings for one language with English as the fallback.
pub struct I18n {
    pub lang: &'static str,
    main: FluentBundle<FluentResource>,
    fallback: FluentBundle<FluentResource>,
}

fn bundle(lang: &str, src: &str) -> FluentBundle<FluentResource> {
    let id = lang.parse().unwrap_or_default();
    let mut b = FluentBundle::new(vec![id]);
    b.set_use_isolating(false);
    match FluentResource::try_new(src.to_owned()) {
        Ok(res) => {
            let _ = b.add_resource(res);
        }
        Err((res, errors)) => {
            tracing::error!("{lang}.ftl: {} parse errors", errors.len());
            let _ = b.add_resource(res);
        }
    }
    b
}

/// "system" | "en" | "ru" → a shipped language code.
pub fn resolve(setting: &str) -> &'static str {
    let lang = match setting {
        "system" => sys_locale::get_locale().unwrap_or_default(),
        other => other.to_owned(),
    };
    if lang.starts_with("ru") { "ru" } else { "en" }
}

impl I18n {
    pub fn new(lang: &'static str) -> Self {
        let src = if lang == "ru" { RU } else { EN };
        Self {
            lang,
            main: bundle(lang, src),
            fallback: bundle("en", EN),
        }
    }

    pub fn t(&self, id: &str) -> String {
        self.fmt(id, None)
    }

    pub fn t_args(&self, id: &str, args: &[(&str, &str)]) -> String {
        let mut fa = FluentArgs::new();
        for (k, v) in args {
            fa.set(*k, FluentValue::from(*v));
        }
        self.fmt(id, Some(&fa))
    }

    fn fmt(&self, id: &str, args: Option<&FluentArgs>) -> String {
        for b in [&self.main, &self.fallback] {
            if let Some(pattern) = b.get_message(id).and_then(|m| m.value()) {
                let mut errors = vec![];
                return b.format_pattern(pattern, args, &mut errors).into_owned();
            }
        }
        id.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every English key must exist in Russian and vice versa.
    #[test]
    fn same_keys() {
        let keys = |src: &str| -> Vec<String> {
            let mut k: Vec<_> = src
                .lines()
                .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim().to_owned()))
                .filter(|k| !k.starts_with('#') && !k.is_empty())
                .collect();
            k.sort();
            k
        };
        assert_eq!(keys(EN), keys(RU));
        let ru = I18n::new("ru");
        assert_eq!(ru.t("menu-view"), "Вид");
        assert_eq!(ru.t_args("appearance-scale-value", &[("percent", "125")]), "125%");
        assert_eq!(ru.t("no-such-key"), "no-such-key");
    }
}
