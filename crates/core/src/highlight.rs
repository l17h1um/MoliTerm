//! Keyword highlighting rules (SPEC §7.6), applied to visible rows at render time.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::theme::Rgb;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Rule {
    pub pattern: String,
    /// `#rrggbb`, empty = keep.
    pub fg: String,
    pub bg: String,
    pub case_insensitive: bool,
    pub enabled: bool,
}

impl Rule {
    fn new(pattern: &str, fg: &str) -> Self {
        Self {
            pattern: pattern.into(),
            fg: fg.into(),
            bg: String::new(),
            case_insensitive: true,
            enabled: true,
        }
    }
}

pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule::new(r"error|fail(ed|ure)?|fatal|denied", "#f07178"),
        Rule::new(r"warn(ing)?", "#ff8f40"),
        Rule::new(r"success|\bok\b|done", "#7fd962"),
    ]
}

pub struct Compiled {
    rules: Vec<(Regex, Option<Rgb>, Option<Rgb>)>,
}

fn color(s: &str) -> Option<Rgb> {
    Rgb::try_from(s.to_owned()).ok()
}

impl Compiled {
    /// Invalid or disabled rules are skipped.
    pub fn new(rules: &[Rule]) -> Self {
        Self {
            rules: rules
                .iter()
                .filter(|r| r.enabled && !r.pattern.is_empty())
                .filter_map(|r| {
                    let re = RegexBuilder::new(&r.pattern)
                        .case_insensitive(r.case_insensitive)
                        .build()
                        .ok()?;
                    Some((re, color(&r.fg), color(&r.bg)))
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Byte ranges in `line` with their (fg, bg). Later rules win on overlap.
    pub fn matches(&self, line: &str) -> Vec<(std::ops::Range<usize>, Option<Rgb>, Option<Rgb>)> {
        let mut out = vec![];
        for (re, fg, bg) in &self.rules {
            for m in re.find_iter(line) {
                if !m.is_empty() {
                    out.push((m.range(), *fg, *bg));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Compiled::new(&default_rules());
        let m = c.matches("Build FAILED: permission denied, warning: ok done");
        let words: Vec<_> = m
            .iter()
            .map(|(r, ..)| &"Build FAILED: permission denied, warning: ok done"[r.clone()])
            .collect();
        assert_eq!(words, ["FAILED", "denied", "warning", "ok", "done"]);
        assert!(c.matches("token okay MacBook-Air").is_empty());
        let bad = Rule::new("(", "#ffffff");
        assert!(Compiled::new(&[bad]).is_empty());
    }
}
