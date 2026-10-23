//! `~/.ssh/known_hosts` (SPEC §5.1.7): hashed and plain entries are read, new
//! entries are appended in plain form. Lines we can't parse are skipped, never fatal.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, Mac};
use russh::keys::{HashAlg, PublicKey};

#[derive(Debug, PartialEq)]
pub enum Check {
    Known,
    Unknown,
    /// Same key type saved with a different key.
    Changed {
        saved: Box<PublicKey>,
    },
}

pub fn default_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".ssh").join("known_hosts"))
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

fn host_port(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

fn host_matches(field: &str, wanted: &str) -> bool {
    field.split(',').any(|entry| {
        if let Some(rest) = entry.strip_prefix("|1|") {
            let mut parts = rest.split('|');
            let (Some(Ok(salt)), Some(Ok(hash))) =
                (parts.next().map(|s| B64.decode(s)), parts.next().map(|s| B64.decode(s)))
            else {
                return false;
            };
            Hmac::<sha1::Sha1>::new_from_slice(&salt)
                .map(|m| m.chain_update(wanted.as_bytes()).verify_slice(&hash).is_ok())
                .unwrap_or(false)
        } else {
            crate::sshconfig::wildcard(entry, wanted)
        }
    })
}

/// (line index, key) for every entry naming this host.
fn entries(text: &str, host: &str, port: u16) -> Vec<(usize, PublicKey)> {
    let wanted = host_port(host, port);
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
                return None;
            }
            let mut f = line.split_whitespace();
            let (hosts, ktype, b64) = (f.next()?, f.next()?, f.next()?);
            if !host_matches(hosts, &wanted) {
                return None;
            }
            PublicKey::from_openssh(&format!("{ktype} {b64}")).ok().map(|k| (i, k))
        })
        .collect()
}

pub fn check(path: &Path, host: &str, port: u16, key: &PublicKey) -> Check {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let found = entries(&text, host, port);
    if found.iter().any(|(_, k)| k.key_data() == key.key_data()) {
        return Check::Known;
    }
    match found.into_iter().find(|(_, k)| k.algorithm() == key.algorithm()) {
        Some((_, saved)) => Check::Changed { saved: Box::new(saved) },
        None => Check::Unknown,
    }
}

fn line_for(host: &str, port: u16, key: &PublicKey) -> std::io::Result<String> {
    let mut k = key.clone();
    k.set_comment("");
    let openssh = k.to_openssh().map_err(std::io::Error::other)?;
    Ok(format!("{} {}\n", host_port(host, port), openssh.trim()))
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn append(path: &Path, host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line_for(host, port, key)?);
    write_private(path, &text)
}

/// Drops this host's entries of the same key type, then appends the new key.
pub fn replace(path: &Path, host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let stale: Vec<usize> = entries(&text, host, port)
        .into_iter()
        .filter(|(_, k)| k.algorithm() == key.algorithm())
        .map(|(i, _)| i)
        .collect();
    let mut kept: String = text
        .lines()
        .enumerate()
        .filter(|(i, _)| !stale.contains(i))
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    kept.push_str(&line_for(host, port, key)?);
    write_private(path, &kept)
}

/// One saved host key, for the Vault page.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// 0-based line in the file (for [`remove_line`]).
    pub line: usize,
    /// Host patterns as written; hashed entries are `None`.
    pub hosts: Option<String>,
    pub key_type: String,
    pub fingerprint: String,
}

pub fn list(path: &Path) -> Vec<Entry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
                return None;
            }
            let mut f = line.split_whitespace();
            let (hosts, ktype, b64) = (f.next()?, f.next()?, f.next()?);
            let key = PublicKey::from_openssh(&format!("{ktype} {b64}")).ok()?;
            Some(Entry {
                line: i,
                hosts: (!hosts.starts_with("|1|")).then(|| hosts.to_owned()),
                key_type: ktype.to_owned(),
                fingerprint: fingerprint(&key),
            })
        })
        .collect()
}

/// Removes one line (by index) from a known-hosts style file.
pub fn remove_line(path: &Path, line: usize) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let kept: String = text
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != line)
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    write_private(path, &kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICBqQvsJXXOPzUhGyVbZQGa5eI4bmGyWYQp4i/vZfBNV";
    const ED_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBWKEp3br1NjjXJ4+FLOQjNdWOdZcY+w7Z3GUKLHd7mD";

    #[test]
    fn check_append_replace_hashed() {
        let dir = std::env::temp_dir().join(format!("moliterm-kh-{}", std::process::id()));
        let path = dir.join("known_hosts");
        let a = PublicKey::from_openssh(ED_A).unwrap();
        let b = PublicKey::from_openssh(ED_B).unwrap();
        assert_eq!(check(&path, "h", 22, &a), Check::Unknown);
        append(&path, "h", 2222, &a).unwrap();
        assert_eq!(check(&path, "h", 2222, &a), Check::Known);
        assert_eq!(check(&path, "h", 22, &a), Check::Unknown);
        assert!(matches!(check(&path, "h", 2222, &b), Check::Changed { .. }));
        replace(&path, "h", 2222, &b).unwrap();
        assert_eq!(check(&path, "h", 2222, &b), Check::Known);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);

        // Hashed entry for "web" with salt "0123456789abcdefghij".
        let salt = b"0123456789abcdefghij";
        let mac = Hmac::<sha1::Sha1>::new_from_slice(salt)
            .unwrap()
            .chain_update(b"web")
            .finalize()
            .into_bytes();
        let line = format!("|1|{}|{} {ED_A}\nbroken line here\n", B64.encode(salt), B64.encode(mac));
        std::fs::write(&path, line).unwrap();
        assert_eq!(check(&path, "web", 22, &a), Check::Known);
        let entries = list(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            (entries[0].hosts.as_deref(), entries[0].key_type.as_str()),
            (None, "ssh-ed25519")
        );
        remove_line(&path, entries[0].line).unwrap();
        assert!(list(&path).is_empty());
        assert!(std::fs::read_to_string(&path).unwrap().contains("broken line"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
