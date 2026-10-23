//! Read-only `~/.ssh/config` support (SPEC §5.1.8): `Host` (with wildcard
//! defaults), `HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`, `Include`.
//! Every concrete `Host` alias becomes one entry with its options resolved the
//! OpenSSH way (first value wins, blocks in file order).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostEntry {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
}

struct Block {
    patterns: Vec<String>,
    opts: Vec<(String, String)>,
}

/// Parses `path` (following `Include`). Missing file → empty list.
pub fn load(path: &Path) -> Vec<HostEntry> {
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    parse(&text, &|pattern| read_includes(&base, pattern))
}

pub fn default_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".ssh").join("config"))
}

fn read_includes(base: &Path, pattern: &str) -> Vec<String> {
    let expanded = expand_home(pattern);
    let full = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        base.join(expanded)
    };
    let (Some(dir), Some(name)) = (
        full.parent(),
        full.file_name().map(|n| n.to_string_lossy().into_owned()),
    ) else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| wildcard(&name, &n.to_string_lossy())))
        .collect();
    files.sort();
    files.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect()
}

pub fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), directories::BaseDirs::new()) {
        (Some(rest), Some(d)) => d.home_dir().join(rest).to_string_lossy().into_owned(),
        _ => p.to_owned(),
    }
}

/// `include` returns the contents of the files an `Include` pattern names.
pub fn parse(text: &str, include: &dyn Fn(&str) -> Vec<String>) -> Vec<HostEntry> {
    let mut blocks = vec![Block {
        patterns: vec!["*".into()],
        opts: vec![],
    }];
    collect(text, include, &mut blocks, 0);

    let mut aliases: Vec<String> = vec![];
    for b in &blocks {
        for p in &b.patterns {
            if !p.contains(['*', '?', '!']) && !aliases.contains(p) {
                aliases.push(p.clone());
            }
        }
    }
    aliases
        .into_iter()
        .map(|alias| {
            let mut e = HostEntry {
                alias: alias.clone(),
                ..Default::default()
            };
            for b in blocks.iter().filter(|b| matches(&b.patterns, &alias)) {
                for (k, v) in &b.opts {
                    match k.as_str() {
                        "hostname" if e.hostname.is_none() => e.hostname = Some(v.clone()),
                        "user" if e.user.is_none() => e.user = Some(v.clone()),
                        "port" if e.port.is_none() => e.port = v.parse().ok(),
                        "identityfile" => e.identity_files.push(v.clone()),
                        "proxyjump" if e.proxy_jump.is_none() && v != "none" => e.proxy_jump = Some(v.clone()),
                        _ => {}
                    }
                }
            }
            e
        })
        .collect()
}

fn collect(text: &str, include: &dyn Fn(&str) -> Vec<String>, blocks: &mut Vec<Block>, depth: u8) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim_start_matches([' ', '\t', '=']).trim()),
            None => continue,
        };
        let value = value.trim_matches('"');
        match key.as_str() {
            "host" => blocks.push(Block {
                patterns: value
                    .split_whitespace()
                    .map(|s| s.trim_matches('"').to_owned())
                    .collect(),
                opts: vec![],
            }),
            // `Match` conditions are not evaluated; its options never apply.
            "match" => blocks.push(Block {
                patterns: vec![],
                opts: vec![],
            }),
            "include" if depth < 8 => {
                for pattern in value.split_whitespace() {
                    for inc in include(pattern) {
                        collect(&inc, include, blocks, depth + 1);
                    }
                }
            }
            _ => {
                if let Some(b) = blocks.last_mut() {
                    b.opts.push((key, value.to_owned()));
                }
            }
        }
    }
}

/// OpenSSH pattern list: any positive match and no negated match.
fn matches(patterns: &[String], host: &str) -> bool {
    let mut hit = false;
    for p in patterns {
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard(neg, host) {
                return false;
            }
        } else if wildcard(p, host) {
            hit = true;
        }
    }
    hit
}

/// `*` and `?` glob.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_and_includes() {
        let main = "\
# comment
Include conf.d/*
Host web-* !web-test
    User deploy
Host web-01 web-02
    HostName 10.20.1.11
    Port=2222
Host bastion
  HostName bastion.example.com
  User admin
  IdentityFile ~/.ssh/id_ed25519
Host db
  ProxyJump bastion
Match host x
  User nobody
Host *
  User fallback
  IdentityFile ~/.ssh/id_rsa
";
        let inc = "Host extra\n  HostName extra.example.com\n";
        let hosts = parse(main, &|p| if p == "conf.d/*" { vec![inc.to_owned()] } else { vec![] });
        let get = |a: &str| hosts.iter().find(|h| h.alias == a).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["extra", "web-01", "web-02", "bastion", "db"]);
        assert_eq!(get("web-01").user.as_deref(), Some("deploy"));
        assert_eq!(get("web-01").port, Some(2222));
        assert_eq!(get("bastion").user.as_deref(), Some("admin"));
        assert_eq!(get("bastion").identity_files, ["~/.ssh/id_ed25519", "~/.ssh/id_rsa"]);
        assert_eq!(get("db").proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(get("db").user.as_deref(), Some("fallback"));
        assert_eq!(get("extra").hostname.as_deref(), Some("extra.example.com"));
    }

    #[test]
    fn globs() {
        assert!(wildcard("web-*", "web-01"));
        assert!(wildcard("w?b", "web"));
        assert!(!wildcard("web-*", "db"));
        assert!(wildcard("*", ""));
        assert!(!matches(&["web-*".into(), "!web-test".into()], "web-test"));
    }
}
