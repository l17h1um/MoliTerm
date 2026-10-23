//! Telnet and raw TCP (SPEC §5.3): own minimal NVT. We agree to ECHO,
//! SUPPRESS-GO-AHEAD, BINARY, NAWS and TERMINAL-TYPE and refuse the rest.

use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use zeroize::Zeroizing;

use crate::{
    ssh::{Ask, Ctx, PromptKind},
    terminal::{Cmd, SessionEvent, Size, Wire},
};

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const BINARY: u8 = 0;
const ECHO: u8 = 1;
const SGA: u8 = 3;
const TTYPE: u8 = 24;
const NAWS: u8 = 31;

/// Telnet settings stored in `Session.extra`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Extra {
    pub auto_login: bool,
    pub login_prompt: String,
    pub password_prompt: String,
}

impl Default for Extra {
    fn default() -> Self {
        Self {
            auto_login: false,
            login_prompt: r"(?i)(login|username|user name)\s*:\s*$".into(),
            password_prompt: r"(?i)password\s*:\s*$".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub host: String,
    pub port: u16,
    /// Raw TCP: no negotiation, bytes as is.
    pub raw: bool,
    pub login: Option<AutoLogin>,
}

#[derive(Clone, Debug)]
pub struct AutoLogin {
    /// Empty = the user types the login.
    pub user: String,
    /// Vault id prefix; the password lives at `<prefix>:password`.
    pub secret_id: String,
    pub login_re: String,
    pub password_re: String,
}

#[derive(Default)]
enum St {
    #[default]
    Data,
    Iac,
    Opt(u8),
    Sb,
    SbIac,
}

/// Telnet protocol state: splits received bytes into data and replies.
pub struct Nvt {
    st: St,
    sb: Vec<u8>,
    /// Options enabled on our side / the server's side.
    us: [bool; 256],
    him: [bool; 256],
    pub size: (u16, u16),
}

impl Default for Nvt {
    fn default() -> Self {
        Self {
            st: St::Data,
            sb: vec![],
            us: [false; 256],
            him: [false; 256],
            size: (80, 24),
        }
    }
}

impl Nvt {
    /// Returns (terminal data, bytes to send back).
    pub fn input(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (mut data, mut out) = (Vec::with_capacity(bytes.len()), vec![]);
        for &b in bytes {
            self.st = match std::mem::take(&mut self.st) {
                St::Data if b == IAC => St::Iac,
                St::Data => {
                    data.push(b);
                    St::Data
                }
                St::Iac => match b {
                    IAC => {
                        data.push(IAC);
                        St::Data
                    }
                    DO | DONT | WILL | WONT => St::Opt(b),
                    SB => {
                        self.sb.clear();
                        St::Sb
                    }
                    _ => St::Data,
                },
                St::Opt(cmd) => {
                    self.negotiate(cmd, b, &mut out);
                    St::Data
                }
                St::Sb if b == IAC => St::SbIac,
                St::Sb => {
                    if self.sb.len() < 512 {
                        self.sb.push(b);
                    }
                    St::Sb
                }
                St::SbIac if b == SE => {
                    self.subneg(&mut out);
                    St::Data
                }
                St::SbIac => {
                    self.sb.push(b);
                    St::Sb
                }
            };
        }
        (data, out)
    }

    fn negotiate(&mut self, cmd: u8, opt: u8, out: &mut Vec<u8>) {
        let i = opt as usize;
        match cmd {
            DO => {
                let ok = matches!(opt, BINARY | SGA | TTYPE | NAWS);
                if !ok {
                    out.extend([IAC, WONT, opt]);
                } else if !self.us[i] {
                    self.us[i] = true;
                    out.extend([IAC, WILL, opt]);
                    if opt == NAWS {
                        out.extend(self.naws());
                    }
                }
            }
            DONT if self.us[i] => {
                self.us[i] = false;
                out.extend([IAC, WONT, opt]);
            }
            WILL => {
                let ok = matches!(opt, BINARY | SGA | ECHO);
                if !ok {
                    out.extend([IAC, DONT, opt]);
                } else if !self.him[i] {
                    self.him[i] = true;
                    out.extend([IAC, DO, opt]);
                }
            }
            WONT if self.him[i] => {
                self.him[i] = false;
                out.extend([IAC, DONT, opt]);
            }
            _ => {}
        }
    }

    fn subneg(&mut self, out: &mut Vec<u8>) {
        // TERMINAL-TYPE SEND → IS xterm-256color
        if self.sb.as_slice() == [TTYPE, 1] {
            out.extend([IAC, SB, TTYPE, 0]);
            out.extend(b"xterm-256color");
            out.extend([IAC, SE]);
        }
    }

    /// NAWS subnegotiation for the current size, or nothing if not agreed.
    pub fn naws(&self) -> Vec<u8> {
        if !self.us[NAWS as usize] {
            return vec![];
        }
        let mut v = vec![IAC, SB, NAWS];
        for n in [self.size.0, self.size.1] {
            for b in n.to_be_bytes() {
                v.push(b);
                if b == IAC {
                    v.push(IAC);
                }
            }
        }
        v.extend([IAC, SE]);
        v
    }

    /// User input → wire: IAC doubled, Enter as CR LF (NVT newline).
    pub fn output(bytes: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(bytes.len() + 2);
        for &b in bytes {
            match b {
                IAC => v.extend([IAC, IAC]),
                b'\r' => v.extend(b"\r\n"),
                _ => v.push(b),
            }
        }
        v
    }
}

/// Watches received text for login/password prompts.
struct Login {
    cfg: AutoLogin,
    login_re: Option<Regex>,
    password_re: Option<Regex>,
    tail: String,
    user_sent: bool,
    done: bool,
}

enum Step {
    None,
    User(String),
    Password,
}

impl Login {
    fn new(cfg: AutoLogin) -> Self {
        Self {
            login_re: Regex::new(&cfg.login_re).ok(),
            password_re: Regex::new(&cfg.password_re).ok(),
            user_sent: cfg.user.is_empty(),
            cfg,
            tail: String::new(),
            done: false,
        }
    }

    fn feed(&mut self, data: &[u8]) -> Step {
        if self.done {
            return Step::None;
        }
        self.tail.push_str(&String::from_utf8_lossy(data));
        if self.tail.len() > 256 {
            let cut = self.tail.len() - 256;
            let cut = (cut..self.tail.len())
                .find(|&i| self.tail.is_char_boundary(i))
                .unwrap_or(0);
            self.tail.drain(..cut);
        }
        let text = self.tail.trim_end_matches(['\0']);
        if !self.user_sent && self.login_re.as_ref().is_some_and(|r| r.is_match(text)) {
            self.user_sent = true;
            self.tail.clear();
            return Step::User(self.cfg.user.clone());
        }
        if self.password_re.as_ref().is_some_and(|r| r.is_match(text)) {
            self.done = true;
            return Step::Password;
        }
        Step::None
    }
}

async fn password(ctx: &Ctx, cfg: &AutoLogin, label: &str) -> Option<Zeroizing<String>> {
    let id = format!("{}:password", cfg.secret_id);
    if let Some(pw) = ctx.secret(id.clone()).await {
        return Some(pw);
    }
    let user = if cfg.user.is_empty() { "?" } else { cfg.user.as_str() };
    let (pw, save) = ctx
        .request(|reply| Ask::Password {
            title: label.to_owned(),
            kind: PromptKind::Password(format!("{user}@{label}")),
            reply,
        })
        .await
        .flatten()?;
    if save {
        ctx.send(Ask::StoreSecret { id, value: pw.clone() });
    }
    Some(pw)
}

/// Runs the session until the user closes the tab or the server hangs up.
pub async fn run(opts: Options, ctx: Ctx, wire: Wire, size: Size) {
    let mut cmds = wire.commands().await;
    let target = format!("{}:{}", opts.host, opts.port);
    wire.event(SessionEvent::Info {
        target: target.clone(),
        detail: if opts.raw { "Raw TCP" } else { "Telnet" }.into(),
    });
    let stream = match tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((opts.host.as_str(), opts.port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return wire.event(SessionEvent::Disconnected(format!("{target}: {e}"))),
        Err(_) => return wire.event(SessionEvent::Disconnected(format!("connection to {target} timed out"))),
    };
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    wire.event(SessionEvent::Connected);
    let mut nvt = Nvt {
        size: (size.cols, size.rows),
        ..Default::default()
    };
    let mut login = opts.login.clone().filter(|_| !opts.raw).map(Login::new);
    let mut buf = vec![0u8; 16 * 1024];
    let reason = loop {
        tokio::select! {
            r = rd.read(&mut buf) => match r {
                Ok(0) => break "connection closed by peer".to_owned(),
                Err(e) => break e.to_string(),
                Ok(n) if opts.raw => wire.feed(&buf[..n]),
                Ok(n) => {
                    let (data, reply) = nvt.input(&buf[..n]);
                    if !reply.is_empty() && wr.write_all(&reply).await.is_err() {
                        break "write failed".to_owned();
                    }
                    wire.feed(&data);
                    let step = login.as_mut().map_or(Step::None, |l| l.feed(&data));
                    let send = match step {
                        Step::None => None,
                        Step::User(u) => Some(Zeroizing::new(format!("{u}\r\n"))),
                        Step::Password => match &login {
                            Some(l) => password(&ctx, &l.cfg, &target).await.map(|p| Zeroizing::new(format!("{}\r\n", p.as_str()))),
                            None => None,
                        },
                    };
                    if let Some(s) = send && wr.write_all(s.as_bytes()).await.is_err() {
                        break "write failed".to_owned();
                    }
                }
            },
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Input(bytes)) => {
                    let out = if opts.raw { bytes } else { Nvt::output(&bytes) };
                    if wr.write_all(&out).await.is_err() {
                        break "write failed".to_owned();
                    }
                }
                Some(Cmd::Resize(s)) => {
                    nvt.size = (s.cols, s.rows);
                    let naws = nvt.naws();
                    if !opts.raw && !naws.is_empty() {
                        let _ = wr.write_all(&naws).await;
                    }
                }
                Some(Cmd::Break) => {}
                Some(Cmd::Close) | None => {
                    let _ = wr.shutdown().await;
                    return;
                }
            },
        }
    };
    wire.event(SessionEvent::Disconnected(reason));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        let mut n = Nvt {
            size: (80, 24),
            ..Default::default()
        };
        // DO TTYPE, DO NAWS, WILL ECHO, DO LINEMODE(34), WILL STATUS(5), text with escaped IAC.
        let (data, out) = n.input(&[
            IAC, DO, TTYPE, IAC, DO, NAWS, IAC, WILL, ECHO, IAC, DO, 34, IAC, WILL, 5, b'h', IAC, IAC, b'i',
        ]);
        assert_eq!(data, [b'h', IAC, b'i']);
        let mut want = vec![IAC, WILL, TTYPE, IAC, WILL, NAWS, IAC, SB, NAWS, 0, 80, 0, 24, IAC, SE];
        want.extend([IAC, DO, ECHO, IAC, WONT, 34, IAC, DONT, 5]);
        assert_eq!(out, want);
        // A repeated DO for an enabled option gets no reply (no loops).
        assert_eq!(n.input(&[IAC, DO, NAWS]).1, Vec::<u8>::new());
        // TTYPE SEND split across chunks.
        assert_eq!(n.input(&[IAC, SB, TTYPE]).1, Vec::<u8>::new());
        let (_, out) = n.input(&[1, IAC, SE]);
        assert_eq!(&out[..4], &[IAC, SB, TTYPE, 0]);
        assert!(out.ends_with(b"xterm-256color\xff\xf0"));
        // NAWS escapes 255.
        n.size = (255, 24);
        assert_eq!(n.naws(), [IAC, SB, NAWS, 0, 255, 255, 0, 24, IAC, SE]);
        assert_eq!(Nvt::output(b"a\r\xff"), b"a\r\n\xff\xff");
    }

    #[test]
    fn login_prompts() {
        let ex = Extra::default();
        let mut l = Login::new(AutoLogin {
            user: "admin".into(),
            secret_id: "s".into(),
            login_re: ex.login_prompt,
            password_re: ex.password_prompt,
        });
        assert!(matches!(l.feed(b"Welcome\r\nrouter log"), Step::None));
        assert!(matches!(l.feed(b"in: "), Step::User(u) if u == "admin"));
        assert!(matches!(l.feed(b"admin\r\nPassword: "), Step::Password));
        assert!(matches!(l.feed(b"Password: "), Step::None));
    }
}
