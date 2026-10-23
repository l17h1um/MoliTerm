//! Macros: named key sequences run in the current tab or the multi-exec
//! targets. The body is text: each line ends with Enter; `{sleep 500}`
//! waits; `{ctrl+c}`, `{enter}`, `{tab}`, `{esc}`, `{up}`… are keys;
//! `\{` and `\\` are literal.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{
    Error,
    db::{Db, now_ms},
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Macro {
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub sort: i64,
    pub body: String,
    /// App shortcut text ("Cmd+Shift+1"); empty = none.
    pub hotkey: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Send(Vec<u8>),
    Sleep(u64),
}

const KEYS: &[(&str, &[u8])] = &[
    ("enter", b"\r"),
    ("tab", b"\t"),
    ("esc", b"\x1b"),
    ("bs", b"\x7f"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
];

/// Body → steps. Unknown `{…}` is sent literally.
pub fn parse(body: &str) -> Vec<Step> {
    let mut steps = vec![];
    let mut cur: Vec<u8> = vec![];
    let mut chars = body.chars().peekable();
    let push = |cur: &mut Vec<u8>, s: &[u8]| cur.extend_from_slice(s);
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('{' | '\\')) => push(&mut cur, e.to_string().as_bytes()),
                Some(e) => push(&mut cur, format!("\\{e}").as_bytes()),
                None => push(&mut cur, b"\\"),
            },
            '\r' => {}
            '\n' => push(&mut cur, b"\r"),
            '{' => {
                let tag: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let t = tag.trim().to_ascii_lowercase();
                if let Some(ms) = t.strip_prefix("sleep").and_then(|v| v.trim().parse::<u64>().ok()) {
                    if !cur.is_empty() {
                        steps.push(Step::Send(std::mem::take(&mut cur)));
                    }
                    steps.push(Step::Sleep(ms));
                } else if let Some(k) = t
                    .strip_prefix("ctrl+")
                    .and_then(|k| k.chars().next().filter(|c| c.is_ascii_alphabetic() && k.len() == 1))
                {
                    cur.push(k.to_ascii_lowercase() as u8 & 0x1f);
                } else if let Some((_, b)) = KEYS.iter().find(|(n, _)| *n == t) {
                    push(&mut cur, b);
                } else {
                    push(&mut cur, format!("{{{tag}}}").as_bytes());
                }
            }
            c => push(&mut cur, c.to_string().as_bytes()),
        }
    }
    if !cur.is_empty() {
        steps.push(Step::Send(cur));
    }
    steps
}

/// Recorded keystrokes → body text (inverse of [`parse`] for what a terminal sends).
pub fn from_keys(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut rest: &str = &text;
    // Longest sequences first, so ESC [ A is {up}, not {esc}[A.
    let mut keys: Vec<&(&str, &[u8])> = KEYS.iter().skip(1).collect();
    keys.sort_by_key(|(_, seq)| std::cmp::Reverse(seq.len()));
    'outer: while let Some(c) = rest.chars().next() {
        for (name, seq) in &keys {
            if let Some(r) = rest.strip_prefix(std::str::from_utf8(seq).unwrap_or("\0")) {
                out.push_str(&format!("{{{name}}}"));
                rest = r;
                continue 'outer;
            }
        }
        match c {
            '\r' => out.push('\n'),
            '{' => out.push_str("\\{"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("{{ctrl+{}}}", ((c as u8) | 0x60) as char)),
            c => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

pub fn load(db: &Db) -> Result<Vec<Macro>, Error> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name, sort, data FROM macros WHERE deleted_at IS NULL ORDER BY sort, name")?;
    let rows = stmt
        .query_map([], |r| {
            let data: String = r.get(3)?;
            let mut m: Macro = serde_json::from_str(&data).unwrap_or_default();
            m.id = r.get(0)?;
            m.name = r.get(1)?;
            m.sort = r.get(2)?;
            Ok(m)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn save(db: &Db, m: &Macro) -> Result<(), Error> {
    db.conn().execute(
        "INSERT INTO macros (id, name, sort, data, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = ?2, sort = ?3, data = ?4, updated_at = ?5, device_id = ?6",
        params![
            m.id,
            m.name,
            m.sort,
            serde_json::to_string(m)?,
            now_ms(),
            db.device_id()
        ],
    )?;
    Ok(())
}

pub fn delete(db: &Db, id: &str) -> Result<(), Error> {
    db.conn().execute(
        "UPDATE macros SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE id = ?1",
        params![id, now_ms(), db.device_id()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_body() {
        assert_eq!(
            parse("sudo -i\n{sleep 800}{ctrl+c}ls \\{x}\\\\ {up}{weird}"),
            vec![
                Step::Send(b"sudo -i\r".to_vec()),
                Step::Sleep(800),
                Step::Send(b"\x03ls {x}\\ \x1b[A{weird}".to_vec()),
            ]
        );
        assert_eq!(parse("a\r\nb"), vec![Step::Send(b"a\rb".to_vec())]);
    }

    #[test]
    fn record_roundtrip() {
        let keys = b"echo {hi}\\\r\x03\x1b[Aq";
        let body = from_keys(keys);
        assert_eq!(body, "echo \\{hi}\\\\\n{ctrl+c}{up}q");
        assert_eq!(parse(&body), vec![Step::Send(keys.to_vec())]);
    }

    #[test]
    fn db_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let m = Macro {
            id: crate::sessions::new_id(),
            name: "update".into(),
            body: "apt update\n".into(),
            hotkey: "Cmd+Shift+1".into(),
            ..Default::default()
        };
        save(&db, &m).unwrap();
        assert_eq!(load(&db).unwrap(), vec![m.clone()]);
        delete(&db, &m.id).unwrap();
        assert!(load(&db).unwrap().is_empty());
    }
}
