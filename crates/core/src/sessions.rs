//! Session tree (SPEC §4.2, §9.4): folders of unlimited depth, sessions, and
//! setting inheritance. Rows are sync-ready (UUIDv7, timestamps, tombstones).

use std::collections::HashMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    Error,
    db::{Db, now_ms},
    ssh::{AgentKind, AuthMethod, Proxy},
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Ssh,
    Telnet,
    Raw,
    Serial,
    Rdp,
    Local,
}

impl Protocol {
    pub fn tag(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH",
            Protocol::Telnet => "TEL",
            Protocol::Raw => "RAW",
            Protocol::Serial => "SER",
            Protocol::Rdp => "RDP",
            Protocol::Local => "LOC",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ssh => 22,
            Protocol::Telnet => 23,
            Protocol::Rdp => 3389,
            _ => 0,
        }
    }
}

/// Settings a session may inherit from its folders. `None` = inherit.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(default)]
pub struct Common {
    pub username: Option<String>,
    pub auth: Option<AuthMethod>,
    pub key_path: Option<String>,
    pub agent: Option<AgentKind>,
    /// ProxyJump-style list: saved session names / `~/.ssh/config` aliases / `user@host:port`.
    pub jump: Option<String>,
    pub proxy: Option<Proxy>,
    pub encoding: Option<String>,
    /// `#rrggbb` environment color.
    pub color: Option<String>,
    pub monitor: Option<bool>,
    pub keepalive: Option<bool>,
    pub reconnect: Option<bool>,
    pub legacy: Option<bool>,
    pub after_login: Option<String>,
    /// `LANG` sent to the remote shell: unset = auto, `off` = don't send.
    pub locale: Option<String>,
    /// SSH X11 forwarding to the local X server.
    pub x11: Option<bool>,
    /// SFTP panel start folder (empty = home).
    pub sftp_dir: Option<String>,
    /// Identity (user + secret) to log in with (SPEC-9 §9.1).
    pub identity: Option<String>,
    pub agent_forward: Option<bool>,
    pub highlight: Option<bool>,
    pub log: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Folder {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub sort: i64,
    pub common: Common,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub folder: Option<String>,
    pub name: String,
    pub sort: i64,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub tags: Vec<String>,
    pub common: Common,
    /// Protocol-specific settings added by later phases (serial, RDP, …).
    pub extra: Map<String, Value>,
}

/// Session data stored in the `data` JSON column.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct SessionData {
    protocol: Protocol,
    host: String,
    port: u16,
    tags: Vec<String>,
    common: Common,
    extra: Map<String, Value>,
}

impl Session {
    /// Protocol settings from `extra` (defaults for missing fields).
    pub fn extra<T: serde::de::DeserializeOwned + Default>(&self) -> T {
        serde_json::from_value(Value::Object(self.extra.clone())).unwrap_or_default()
    }

    pub fn set_extra<T: Serialize>(&mut self, value: &T) {
        if let Ok(Value::Object(m)) = serde_json::to_value(value) {
            self.extra.extend(m);
        }
    }
}

/// Quick connect (SPEC-9 §9.6): `[ssh|rdp|telnet://][user@]host[:port]`;
/// port 3389 without a scheme means RDP. The session is not saved.
pub fn parse_quick(input: &str) -> Option<Session> {
    let input = input.trim();
    let (scheme, rest) = match input.split_once("://") {
        Some((s, r)) => (Some(s.to_ascii_lowercase()), r),
        None => (None, input),
    };
    let rest = rest.trim_end_matches('/');
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_owned()).filter(|u| !u.is_empty()), h),
        None => (None, rest),
    };
    // `[::1]:22` keeps the brackets out of the host.
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && p.parse::<u16>().is_ok() => (h, p.parse::<u16>().ok()),
        _ => (hostport, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    let protocol = match (scheme.as_deref(), port) {
        (Some("rdp"), _) | (None, Some(3389)) => Protocol::Rdp,
        (Some("telnet"), _) => Protocol::Telnet,
        (Some("ssh") | None, _) => Protocol::Ssh,
        _ => return None,
    };
    Some(Session {
        id: new_id(),
        name: input.to_owned(),
        protocol,
        host: host.to_owned(),
        port: port.unwrap_or(protocol.default_port()),
        common: Common {
            username: user,
            ..Default::default()
        },
        ..Default::default()
    })
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// All live folders and sessions, ordered by `sort` then name.
pub fn load(db: &Db) -> Result<(Vec<Folder>, Vec<Session>), Error> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, parent_id, name, sort, data FROM folders WHERE deleted_at IS NULL ORDER BY sort, name")?;
    let folders = stmt
        .query_map([], |r| {
            let data: String = r.get(4)?;
            Ok(Folder {
                id: r.get(0)?,
                parent: r.get(1)?,
                name: r.get(2)?,
                sort: r.get(3)?,
                common: serde_json::from_str(&data).unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = db
        .conn()
        .prepare("SELECT id, folder_id, name, sort, data FROM sessions WHERE deleted_at IS NULL ORDER BY sort, name")?;
    let sessions = stmt
        .query_map([], |r| {
            let data: String = r.get(4)?;
            let d: SessionData = serde_json::from_str(&data).unwrap_or_default();
            Ok(Session {
                id: r.get(0)?,
                folder: r.get(1)?,
                name: r.get(2)?,
                sort: r.get(3)?,
                protocol: d.protocol,
                host: d.host,
                port: d.port,
                tags: d.tags,
                common: d.common,
                extra: d.extra,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((folders, sessions))
}

pub fn save_folder(db: &Db, f: &Folder) -> Result<(), Error> {
    let now = now_ms();
    db.conn().execute(
        "INSERT INTO folders (id, parent_id, name, sort, data, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET parent_id = ?2, name = ?3, sort = ?4, data = ?5, updated_at = ?6, device_id = ?7",
        params![f.id, f.parent, f.name, f.sort, serde_json::to_string(&f.common)?, now, db.device_id()],
    )?;
    Ok(())
}

pub fn save_session(db: &Db, s: &Session) -> Result<(), Error> {
    let data = SessionData {
        protocol: s.protocol,
        host: s.host.clone(),
        port: s.port,
        tags: s.tags.clone(),
        common: s.common.clone(),
        extra: s.extra.clone(),
    };
    let now = now_ms();
    db.conn().execute(
        "INSERT INTO sessions (id, folder_id, name, sort, data, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET folder_id = ?2, name = ?3, sort = ?4, data = ?5, updated_at = ?6, device_id = ?7",
        params![s.id, s.folder, s.name, s.sort, serde_json::to_string(&data)?, now, db.device_id()],
    )?;
    Ok(())
}

pub fn delete_session(db: &Db, id: &str) -> Result<(), Error> {
    db.conn().execute(
        "UPDATE sessions SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE id = ?1",
        params![id, now_ms(), db.device_id()],
    )?;
    Ok(())
}

/// Tombstones the folder, its subfolders and every session inside.
pub fn delete_folder(db: &Db, id: &str, folders: &[Folder]) -> Result<(), Error> {
    let mut ids = vec![id.to_owned()];
    let mut i = 0;
    while i < ids.len() {
        let parent = ids[i].clone();
        ids.extend(
            folders
                .iter()
                .filter(|f| f.parent.as_deref() == Some(&parent))
                .map(|f| f.id.clone()),
        );
        i += 1;
    }
    let (now, dev) = (now_ms(), db.device_id().to_owned());
    for fid in &ids {
        db.conn().execute(
            "UPDATE folders SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE id = ?1",
            params![fid, now, dev],
        )?;
        db.conn().execute(
            "UPDATE sessions SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE folder_id = ?1 AND deleted_at IS NULL",
            params![fid, now, dev],
        )?;
    }
    Ok(())
}

/// Folders from `id` up to the root, nearest first. Stops on cycles.
pub fn chain<'a>(folders: &'a [Folder], mut id: Option<&'a str>) -> Vec<&'a Folder> {
    let mut out: Vec<&Folder> = vec![];
    while let Some(fid) = id {
        let Some(f) = folders.iter().find(|f| f.id == fid) else {
            break;
        };
        if out.iter().any(|x| x.id == f.id) {
            break;
        }
        out.push(f);
        id = f.parent.as_deref();
    }
    out
}

/// `Production / web`
pub fn path(folders: &[Folder], id: Option<&str>) -> String {
    let mut names: Vec<&str> = chain(folders, id).iter().map(|f| f.name.as_str()).collect();
    names.reverse();
    names.join(" / ")
}

/// Effective settings and, per field, the folder name it came from.
pub struct Resolved {
    pub common: Common,
    pub from: HashMap<String, String>,
}

/// Merges `own` with the folder chain starting at `folder` (nearest wins).
pub fn resolve(own: &Common, folders: &[Folder], folder: Option<&str>) -> Resolved {
    let mut acc = match serde_json::to_value(own) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut from = HashMap::new();
    for f in chain(folders, folder) {
        let Ok(Value::Object(level)) = serde_json::to_value(&f.common) else {
            continue;
        };
        for (k, v) in level {
            if !v.is_null() && acc.get(&k).is_none_or(Value::is_null) {
                from.insert(k.clone(), f.name.clone());
                acc.insert(k, v);
            }
        }
    }
    Resolved {
        common: serde_json::from_value(Value::Object(acc)).unwrap_or_default(),
        from,
    }
}

/// Lower-case match on name, host, username and tags.
pub fn matches(s: &Session, folders: &[Folder], query: &str) -> bool {
    let q = query.to_lowercase();
    let user = resolve(&s.common, folders, s.folder.as_deref())
        .common
        .username
        .unwrap_or_default();
    [s.name.as_str(), s.host.as_str(), user.as_str()]
        .iter()
        .any(|v| v.to_lowercase().contains(&q))
        || s.tags.iter().any(|t| t.to_lowercase().contains(&q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_connect() {
        let s = parse_quick("deploy@web01:2200").unwrap();
        assert_eq!((s.protocol, s.host.as_str(), s.port), (Protocol::Ssh, "web01", 2200));
        assert_eq!(s.common.username.as_deref(), Some("deploy"));
        let s = parse_quick("rdp://CORP\\bob@dc1").unwrap();
        assert_eq!(
            (s.protocol, s.port, s.common.username.as_deref()),
            (Protocol::Rdp, 3389, Some("CORP\\bob"))
        );
        assert_eq!(parse_quick("10.0.0.5:3389").unwrap().protocol, Protocol::Rdp);
        assert_eq!(parse_quick("telnet://switch").unwrap().port, 23);
        assert_eq!(parse_quick("host").unwrap().port, 22);
        assert!(parse_quick("").is_none());
        assert!(parse_quick("ftp://x").is_none());
    }

    fn folder(id: &str, parent: Option<&str>, name: &str, common: Common) -> Folder {
        Folder {
            id: id.into(),
            parent: parent.map(Into::into),
            name: name.into(),
            sort: 0,
            common,
        }
    }

    #[test]
    fn inheritance_nearest_wins() {
        let folders = vec![
            folder(
                "p",
                None,
                "Production",
                Common {
                    username: Some("admin".into()),
                    jump: Some("bastion.prod".into()),
                    color: Some("#f07178".into()),
                    ..Default::default()
                },
            ),
            folder(
                "w",
                Some("p"),
                "web",
                Common {
                    username: Some("deploy".into()),
                    ..Default::default()
                },
            ),
        ];
        let own = Common {
            key_path: Some("~/.ssh/id".into()),
            ..Default::default()
        };
        let r = resolve(&own, &folders, Some("w"));
        assert_eq!(r.common.username.as_deref(), Some("deploy"));
        assert_eq!(r.from["username"], "web");
        assert_eq!(r.common.jump.as_deref(), Some("bastion.prod"));
        assert_eq!(r.from["jump"], "Production");
        assert_eq!(r.common.key_path.as_deref(), Some("~/.ssh/id"));
        assert!(!r.from.contains_key("key_path"));
        assert_eq!(path(&folders, Some("w")), "Production / web");
    }

    #[test]
    fn db_roundtrip_and_tombstones() {
        let db = Db::open_in_memory().unwrap();
        let f = Folder {
            id: new_id(),
            name: "Staging".into(),
            ..Default::default()
        };
        save_folder(&db, &f).unwrap();
        let s = Session {
            id: new_id(),
            folder: Some(f.id.clone()),
            name: "stage-app".into(),
            host: "10.0.0.5".into(),
            port: 22,
            tags: vec!["web".into()],
            ..Default::default()
        };
        save_session(&db, &s).unwrap();
        let (fs, ss) = load(&db).unwrap();
        assert_eq!(fs, vec![f.clone()]);
        assert_eq!(ss, vec![s.clone()]);
        assert!(matches(&s, &fs, "WEB"));
        delete_folder(&db, &f.id, &fs).unwrap();
        let (fs, ss) = load(&db).unwrap();
        assert!(fs.is_empty() && ss.is_empty());
        let tomb: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions WHERE deleted_at IS NOT NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tomb, 1);
    }
}
