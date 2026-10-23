//! Session import (SPEC §9.5): MobaXterm (`MobaXterm.ini` / `.mxtsessions`)
//! and PuTTY (`.reg` export, `~/.putty/sessions`, Windows registry via
//! `reg export`). Passwords are never imported.

use std::collections::BTreeMap;

use crate::{
    serial,
    sessions::{Common, Folder, Protocol, Session, new_id},
};

/// Parsed sessions, ready to be saved under a new folder.
#[derive(Debug, Default)]
pub struct Imported {
    /// Folder paths (`a\b`) in file order, relative to the import root.
    pub folders: Vec<String>,
    /// (folder path or "", session)
    pub sessions: Vec<(String, Session)>,
    /// (name, type) of entries we can't import.
    pub skipped: Vec<(String, String)>,
}

impl Imported {
    /// Sessions per protocol tag, for the preview.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for (_, s) in &self.sessions {
            *m.entry(s.protocol.tag()).or_insert(0) += 1;
        }
        m
    }

    /// Folders and sessions placed under a new root folder `root`.
    pub fn into_tree(self, root: &str) -> (Vec<Folder>, Vec<Session>) {
        let root_id = new_id();
        let mut folders = vec![Folder {
            id: root_id.clone(),
            parent: None,
            name: root.to_owned(),
            sort: i64::MAX / 4,
            common: Common::default(),
        }];
        let mut ids: BTreeMap<String, String> = BTreeMap::new();
        ids.insert(String::new(), root_id.clone());
        let mut ensure = |path: &str, folders: &mut Vec<Folder>| -> String {
            let mut parent = root_id.clone();
            let mut acc = String::new();
            for part in path.split('\\').filter(|p| !p.is_empty()) {
                if !acc.is_empty() {
                    acc.push('\\');
                }
                acc.push_str(part);
                parent = match ids.get(&acc) {
                    Some(id) => id.clone(),
                    None => {
                        let id = new_id();
                        folders.push(Folder {
                            id: id.clone(),
                            parent: Some(parent.clone()),
                            name: part.to_owned(),
                            sort: folders.len() as i64,
                            common: Common::default(),
                        });
                        ids.insert(acc.clone(), id.clone());
                        id
                    }
                };
            }
            parent
        };
        for f in &self.folders {
            ensure(f, &mut folders);
        }
        let mut sessions = vec![];
        for (i, (path, mut s)) in self.sessions.into_iter().enumerate() {
            s.folder = Some(ensure(&path, &mut folders));
            s.sort = i as i64;
            sessions.push(s);
        }
        (folders, sessions)
    }
}

/// Bytes → text: UTF-16 (BOM), UTF-8, else the given legacy code page
/// (MobaXterm/regedit write the Windows ANSI code page).
pub fn decode(bytes: &[u8], legacy: &str) -> String {
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc.decode(bytes).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => encoding_rs::Encoding::for_label(legacy.as_bytes())
            .unwrap_or(encoding_rs::WINDOWS_1252)
            .decode(bytes)
            .0
            .into_owned(),
    }
}

fn session(name: &str, protocol: Protocol, host: &str, port: u16) -> Session {
    Session {
        id: new_id(),
        name: name.trim().to_owned(),
        protocol,
        host: host.trim().to_owned(),
        port,
        ..Default::default()
    }
}

/// MobaXterm escapes inside setting strings.
fn moba_unescape(s: &str) -> String {
    s.replace("__PTVIRG__", ";")
        .replace("__DBLQUO__", "\"")
        .replace("__PIPE__", "|")
        .replace("__PERCENT__", "%")
        .replace("_CurrentDrive_", "C")
}

/// `[Bookmarks*]` sections of `MobaXterm.ini` or a `.mxtsessions` export.
pub fn mobaxterm(text: &str) -> Imported {
    let mut out = Imported::default();
    let mut in_bookmarks = false;
    let mut folder = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_bookmarks = sec == "Bookmarks" || sec.starts_with("Bookmarks_");
            folder.clear();
            continue;
        }
        if !in_bookmarks {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "SubRep" => {
                folder = value.trim().to_owned();
                if !folder.is_empty() && !out.folders.contains(&folder) {
                    out.folders.push(folder.clone());
                }
                continue;
            }
            "ImgNum" => continue,
            _ => {}
        }
        // name = <prefix>#<icon>#<type>%host%port%user%…#<terminal>#…
        let parts: Vec<&str> = value.split('#').collect();
        let Some(f) = parts
            .get(2)
            .map(|g| g.split('%').map(moba_unescape).collect::<Vec<String>>())
        else {
            continue;
        };
        let get = |i: usize| f.get(i).map(String::as_str).unwrap_or("");
        let port = |i: usize, d: u16| get(i).trim().parse().unwrap_or(d);
        let user = |i: usize| Some(get(i).trim().to_owned()).filter(|u| !u.is_empty() && u != "<default>");
        let s = match get(0) {
            "0" => {
                let mut s = session(key, Protocol::Ssh, get(1), port(2, 22));
                s.common.username = user(3);
                s.common.after_login = Some(get(7).to_owned()).filter(|c| !c.trim().is_empty());
                s.common.key_path = Some(get(14).to_owned()).filter(|k| !k.trim().is_empty());
                // Gateways: hosts / ports / users separated by "|".
                let hosts: Vec<&str> = get(8).split('|').filter(|h| !h.is_empty()).collect();
                if !hosts.is_empty() {
                    let ports: Vec<&str> = get(9).split('|').collect();
                    let users: Vec<&str> = get(10).split('|').collect();
                    let jumps: Vec<String> = hosts
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let u = users.get(i).filter(|u| !u.is_empty()).map(|u| format!("{u}@"));
                            let p = ports
                                .get(i)
                                .filter(|p| !p.is_empty() && **p != "22")
                                .map(|p| format!(":{p}"));
                            format!("{}{h}{}", u.unwrap_or_default(), p.unwrap_or_default())
                        })
                        .collect();
                    s.common.jump = Some(jumps.join(", "));
                }
                s
            }
            "1" => {
                let mut s = session(key, Protocol::Telnet, get(1), port(2, 23));
                s.common.username = user(3);
                s
            }
            "4" => {
                let mut s = session(key, Protocol::Rdp, get(1), port(2, 3389));
                s.common.username = user(3);
                s
            }
            // ponytail: serial field positions are inferred (port name, speed); others keep defaults.
            "8" => {
                let dev = get(1).split_whitespace().next().unwrap_or("").to_owned();
                let mut s = session(key, Protocol::Serial, &dev, 0);
                s.set_extra(&serial::Extra {
                    baud: get(2).trim().parse().unwrap_or(9600),
                    ..Default::default()
                });
                s
            }
            "10" => session(key, Protocol::Local, "", 0),
            t => {
                let kind = match t {
                    "2" => "Rsh",
                    "3" => "XDMCP",
                    "5" => "VNC",
                    "6" => "FTP",
                    "7" => "SFTP",
                    "9" => "File",
                    "11" => "Browser",
                    "12" => "Mosh",
                    "13" => "S3",
                    "14" => "WSL",
                    _ => t,
                };
                out.skipped.push((key.trim().to_owned(), kind.to_owned()));
                continue;
            }
        };
        if s.protocol != Protocol::Local && s.host.is_empty() {
            out.skipped.push((key.trim().to_owned(), s.protocol.tag().to_owned()));
            continue;
        }
        out.sessions.push((folder.clone(), s));
    }
    out
}

/// PuTTY settings of one saved session (registry values or ~/.putty file).
fn putty_session(name: &str, v: &BTreeMap<String, String>, out: &mut Imported) {
    let get = |k: &str| v.get(k).map(String::as_str).unwrap_or("");
    let num = |k: &str, d: u32| get(k).trim().parse::<u32>().unwrap_or(d);
    let proto = get("Protocol");
    let (user_in_host, host) = match get("HostName").rsplit_once('@') {
        Some((u, h)) => (Some(u.to_owned()), h.to_owned()),
        None => (None, get("HostName").to_owned()),
    };
    let mut s = match proto {
        "ssh" | "" => session(name, Protocol::Ssh, &host, num("PortNumber", 22) as u16),
        "telnet" => session(name, Protocol::Telnet, &host, num("PortNumber", 23) as u16),
        "raw" => session(name, Protocol::Raw, &host, num("PortNumber", 0) as u16),
        "serial" => {
            let mut s = session(name, Protocol::Serial, get("SerialLine"), 0);
            s.set_extra(&serial::Extra {
                baud: num("SerialSpeed", 9600),
                data_bits: num("SerialDataBits", 8) as u8,
                stop_bits: if num("SerialStopHalfbits", 2) >= 4 { 2 } else { 1 },
                parity: match num("SerialParity", 0) {
                    1 => serial::Parity::Odd,
                    2 => serial::Parity::Even,
                    _ => serial::Parity::None,
                },
                flow: match num("SerialFlowControl", 1) {
                    1 => serial::Flow::XonXoff,
                    2 => serial::Flow::RtsCts,
                    _ => serial::Flow::None,
                },
                ..Default::default()
            });
            s
        }
        other => {
            out.skipped.push((name.to_owned(), other.to_owned()));
            return;
        }
    };
    s.common.username = Some(get("UserName").to_owned())
        .filter(|u| !u.is_empty())
        .or(user_in_host);
    s.common.key_path = Some(get("PublicKeyFile").to_owned()).filter(|k| !k.is_empty());
    // "UTF-8", "CP1251", "KOI8-R", "ISO-8859-1:1998 (Latin-1, West Europe)", …
    let cp = get("LineCodePage").split([' ', ':']).next().unwrap_or("").to_owned();
    if crate::terminal::encoding(Some(&cp)).is_some() {
        s.common.encoding = Some(cp);
    }
    if s.protocol != Protocol::Serial && s.host.is_empty() {
        out.skipped.push((name.to_owned(), proto.to_owned()));
        return;
    }
    out.sessions.push((String::new(), s));
}

/// PuTTY's session names are URL-encoded (`My%20Server`).
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A regedit `.reg` export of `HKCU\Software\SimonTatham\PuTTY\Sessions`.
pub fn putty_reg(text: &str) -> Imported {
    let mut out = Imported::default();
    let mut current: Option<(String, BTreeMap<String, String>)> = None;
    let flush = |cur: &mut Option<(String, BTreeMap<String, String>)>, out: &mut Imported| {
        if let Some((name, v)) = cur.take()
            && name != "Default Settings"
        {
            putty_session(&name, &v, out);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(key) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&mut current, &mut out);
            current = key
                .split_once("\\PuTTY\\Sessions\\")
                .map(|(_, n)| (url_decode(n), BTreeMap::new()));
            continue;
        }
        let Some((_, vals)) = current.as_mut() else {
            continue;
        };
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim_matches('"').to_owned();
        let v = if let Some(hex) = v.strip_prefix("dword:") {
            u32::from_str_radix(hex.trim(), 16).unwrap_or(0).to_string()
        } else {
            v.trim_matches('"')
                .replace("\\\\", "\u{0}")
                .replace("\\\"", "\"")
                .replace('\u{0}', "\\")
        };
        vals.insert(k, v);
    }
    flush(&mut current, &mut out);
    out
}

/// `~/.putty/sessions/<name>` files (`Key=Value` lines), as on Linux.
pub fn putty_dir(dir: &std::path::Path) -> Imported {
    let mut out = Imported::default();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for p in files {
        let Some(name) = p.file_name().map(|n| url_decode(&n.to_string_lossy())) else {
            continue;
        };
        if name == "Default Settings" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let v: BTreeMap<String, String> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        putty_session(&name, &v, &mut out);
    }
    out
}

/// Windows: PuTTY's registry key exported with `reg export` and parsed as a `.reg`.
#[cfg(windows)]
pub fn putty_registry() -> Imported {
    let tmp = std::env::temp_dir().join(format!("moliterm-putty-{}.reg", std::process::id()));
    let ok = std::process::Command::new("reg")
        .args(["export", r"HKCU\Software\SimonTatham\PuTTY\Sessions"])
        .arg(&tmp)
        .arg("/y")
        .status()
        .is_ok_and(|s| s.success());
    let out = match (ok, std::fs::read(&tmp)) {
        (true, Ok(b)) => putty_reg(&decode(&b, "windows-1252")),
        _ => Imported::default(),
    };
    let _ = std::fs::remove_file(&tmp);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn mobaxterm_ini() {
        let text = decode(&fixture("MobaXterm.ini"), "windows-1251");
        let imp = mobaxterm(&text);
        let names: Vec<&str> = imp.sessions.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["web-01", "Сервер БД", "core-sw", "win-dc", "console", "Local bash"]
        );
        assert_eq!(imp.folders, ["Production", "Production\\DB", "Network"]);
        let (path, web) = &imp.sessions[0];
        assert_eq!(path, "");
        assert_eq!(
            (web.protocol, web.host.as_str(), web.port),
            (Protocol::Ssh, "10.0.0.11", 22)
        );
        assert_eq!(web.common.username.as_deref(), Some("deploy"));
        assert_eq!(web.common.after_login.as_deref(), Some("sudo -i; cd /srv"));
        assert_eq!(web.common.key_path.as_deref(), Some("C:\\keys\\id.ppk"));
        assert_eq!(web.common.jump.as_deref(), Some("jump@bastion.example.com:2222"));
        let (path, db) = &imp.sessions[1];
        assert_eq!(path, "Production\\DB");
        assert_eq!((db.port, db.common.username.clone()), (2200, None));
        assert_eq!(imp.sessions[2].1.protocol, Protocol::Telnet);
        let rdp = &imp.sessions[3].1;
        assert_eq!(
            (rdp.protocol, rdp.port, rdp.common.username.as_deref()),
            (Protocol::Rdp, 3389, Some("CORP\\admin"))
        );
        let ser = &imp.sessions[4].1;
        assert_eq!((ser.protocol, ser.host.as_str()), (Protocol::Serial, "COM3"));
        assert_eq!(ser.extra::<serial::Extra>().baud, 115200);
        assert_eq!(imp.sessions[5].1.protocol, Protocol::Local);
        assert_eq!(imp.skipped, [("vnc-box".to_owned(), "VNC".to_owned())]);

        let (folders, sessions) = imp.into_tree("Imported from MobaXterm");
        assert_eq!(folders.len(), 4);
        let db_folder = folders.iter().find(|f| f.name == "DB").unwrap();
        let prod = folders.iter().find(|f| f.name == "Production").unwrap();
        assert_eq!(db_folder.parent.as_ref(), Some(&prod.id));
        assert_eq!(sessions[1].folder.as_ref(), Some(&db_folder.id));
    }

    #[test]
    fn putty_reg_utf16() {
        let text = decode(&fixture("putty.reg"), "windows-1252");
        let imp = putty_reg(&text);
        let names: Vec<&str> = imp.sessions.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, ["My Server", "router", "usb-console"]);
        let s = &imp.sessions[0].1;
        assert_eq!((s.protocol, s.host.as_str(), s.port), (Protocol::Ssh, "10.1.1.1", 2222));
        assert_eq!(s.common.username.as_deref(), Some("root"));
        assert_eq!(s.common.key_path.as_deref(), Some("C:\\Users\\me\\key.ppk"));
        assert_eq!(s.common.encoding.as_deref(), Some("CP1251"));
        assert_eq!(imp.sessions[1].1.protocol, Protocol::Telnet);
        let ser = imp.sessions[2].1.extra::<serial::Extra>();
        assert_eq!((imp.sessions[2].1.host.as_str(), ser.baud), ("COM4", 9600));
        assert_eq!((ser.flow, ser.parity), (serial::Flow::None, serial::Parity::Even));
        assert_eq!(imp.skipped, [("old-rlogin".to_owned(), "rlogin".to_owned())]);
    }

    #[test]
    fn putty_files() {
        let dir = std::env::temp_dir().join(format!("moliterm-putty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("db%20box"),
            "HostName=admin@db.lan\nPortNumber=22\nProtocol=ssh\n",
        )
        .unwrap();
        std::fs::write(dir.join("Default%20Settings"), "HostName=\n").unwrap();
        let imp = putty_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(imp.sessions.len(), 1);
        let s = &imp.sessions[0].1;
        assert_eq!((s.name.as_str(), s.host.as_str()), ("db box", "db.lan"));
        assert_eq!(s.common.username.as_deref(), Some("admin"));
    }
}
