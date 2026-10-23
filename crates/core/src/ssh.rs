//! SSH sessions over russh (SPEC §5.1): direct / proxy / jump-host transport,
//! known_hosts checks, publickey (file, built-in agent, system agent), password
//! and keyboard-interactive auth, shell with PTY, keepalive and auto-reconnect.
//!
//! Anything that needs the user (host key decisions, passwords, vault secrets)
//! goes to the UI as an [`Ask`] with a oneshot reply, so this task never touches
//! UI state directly.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use russh::{
    ChannelMsg, Preferred, cipher,
    client::{self, AuthResult, KeyboardInteractiveAuthResponse},
    kex,
    keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, agent::client::AgentClient},
    mac,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, oneshot},
};
use zeroize::Zeroizing;

use crate::{
    known_hosts::{self, Check},
    terminal::{Cmd, SessionEvent, Size, Wire},
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Password,
    Key,
    Agent,
    KeyboardInteractive,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    #[default]
    System,
    BuiltIn,
    None,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proxy {
    Socks5 { host: String, port: u16 },
    Http { host: String, port: u16 },
}

/// One SSH hop, fully resolved.
#[derive(Clone, Debug)]
pub struct Hop {
    /// Shown in prompts.
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub key_path: Option<String>,
    pub agent: AgentKind,
    /// Vault id prefix, e.g. `session:<uuid>`; secrets live at `<prefix>:password`
    /// and `<prefix>:passphrase`.
    pub secret_id: String,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub target: Hop,
    pub jumps: Vec<Hop>,
    pub proxy: Option<Proxy>,
    pub keepalive: bool,
    pub reconnect: bool,
    pub legacy: bool,
    pub after_login: Option<String>,
    pub agent_forward: bool,
    pub known_hosts: PathBuf,
    pub notices: Notices,
    /// `LANG` to request for the remote shell; `None` = don't send,
    /// `""` = auto (a UTF-8 locale the server actually has, see [`pick_lang`]).
    pub lang: Option<String>,
    /// X11 forwarding: (local display, `""` = `$DISPLAY`; hex fake cookie). `None` = off.
    pub x11: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyAnswer {
    Reject,
    Once,
    /// Unknown: add to known_hosts. Changed: replace the saved entry.
    Save,
}

/// Questions for the UI. Dropping the reply sender counts as "cancel".
pub enum Ask {
    HostKey {
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        /// `Some(saved fingerprint)` when the key changed.
        changed: Option<String>,
        reply: oneshot::Sender<HostKeyAnswer>,
    },
    /// Read a vault secret. `None` if missing or the user keeps the vault locked.
    Secret {
        id: String,
        reply: oneshot::Sender<Option<Zeroizing<String>>>,
    },
    /// Store a secret the user chose to save.
    StoreSecret { id: String, value: Zeroizing<String> },
    /// Password or key passphrase prompt. Reply: (value, save to vault).
    Password {
        title: String,
        kind: PromptKind,
        reply: oneshot::Sender<Option<(Zeroizing<String>, bool)>>,
    },
    KeyboardInteractive {
        title: String,
        instructions: String,
        prompts: Vec<(String, bool)>,
        reply: oneshot::Sender<Option<Vec<Zeroizing<String>>>>,
    },
}

#[derive(Debug, Clone)]
pub enum PromptKind {
    /// `user@host`
    Password(String),
    /// Key file path.
    Passphrase(String),
}

/// Localized separator lines printed into the terminal; `{time}`, `{reason}`,
/// `{n}`, `{secs}` are replaced.
#[derive(Clone, Debug, Default)]
pub struct Notices {
    pub reconnected: String,
    pub retrying: String,
    /// Printed when the server refuses `LANG`; `{lang}` is replaced.
    pub lang_refused: String,
}

/// Keys held by the built-in agent (cleared when the vault locks).
pub type AgentKeys = Arc<Mutex<Vec<Arc<PrivateKey>>>>;

#[derive(Clone)]
pub struct Ctx {
    pub ask: mpsc::UnboundedSender<Ask>,
    pub agent_keys: AgentKeys,
    /// Repaints the UI so it sees the question.
    pub wake: crate::terminal::Waker,
}

impl Ctx {
    pub fn send(&self, ask: Ask) {
        let _ = self.ask.send(ask);
        (self.wake)();
    }

    pub(crate) async fn request<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Ask) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.ask.send(make(tx)).ok()?;
        (self.wake)();
        rx.await.ok()
    }

    pub(crate) async fn secret(&self, id: String) -> Option<Zeroizing<String>> {
        self.request(|reply| Ask::Secret { id, reply }).await.flatten()
    }
}

/// Connection failure. `fatal` = retrying cannot help (auth/host key rejected).
#[derive(Debug)]
pub struct Failure {
    pub msg: String,
    pub fatal: bool,
}

impl Failure {
    fn retry(msg: impl ToString) -> Self {
        Self {
            msg: msg.to_string(),
            fatal: false,
        }
    }
    fn fatal(msg: impl ToString) -> Self {
        Self {
            msg: msg.to_string(),
            fatal: true,
        }
    }
}

// ---- handler ---------------------------------------------------------------

pub struct Client {
    host: String,
    port: u16,
    /// Host key type seen during key exchange, for the status bar.
    key_type: Arc<Mutex<String>>,
    known_hosts: PathBuf,
    ctx: Ctx,
    agent_forward: bool,
    /// Remote-forward (-R) connections go here; `None` = refuse them.
    forwarded: Option<mpsc::UnboundedSender<russh::Channel<client::Msg>>>,
    /// Local X server and fake cookie for forwarded X11 channels; `None` = refuse them.
    x11: Option<(crate::x11::Display, String)>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key: PublicKey = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            russh::keys::PublicKeyOrCertificate::Certificate(c) => PublicKey::from(c.public_key().clone()),
        };
        if let Ok(mut t) = self.key_type.lock() {
            *t = key.algorithm().to_string();
        }
        let check = known_hosts::check(&self.known_hosts, &self.host, self.port, &key);
        let changed = match &check {
            Check::Known => return Ok(true),
            Check::Unknown => None,
            Check::Changed { saved } => Some(known_hosts::fingerprint(saved)),
        };
        let (host, port, is_changed) = (self.host.clone(), self.port, changed.is_some());
        let answer = self
            .ctx
            .request(|reply| Ask::HostKey {
                host,
                port,
                key_type: key.algorithm().to_string(),
                fingerprint: known_hosts::fingerprint(&key),
                changed,
                reply,
            })
            .await
            .unwrap_or(HostKeyAnswer::Reject);
        match answer {
            HostKeyAnswer::Reject => Ok(false),
            HostKeyAnswer::Once => Ok(true),
            HostKeyAnswer::Save => {
                let r = if is_changed {
                    known_hosts::replace(&self.known_hosts, &self.host, self.port, &key)
                } else {
                    known_hosts::append(&self.known_hosts, &self.host, self.port, &key)
                };
                if let Err(e) = r {
                    tracing::warn!("known_hosts write failed: {e}");
                }
                Ok(true)
            }
        }
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if !self.agent_forward {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        let keys = self.ctx.agent_keys.clone();
        tokio::spawn(crate::agent::serve(channel.into_stream(), keys));
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        match self.x11.clone() {
            Some((display, cookie)) => {
                reply.accept().await;
                tokio::spawn(crate::x11::serve(channel.into_stream(), display, cookie));
            }
            None => {
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await
            }
        }
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        match &self.forwarded {
            Some(tx) => {
                reply.accept().await;
                let _ = tx.send(channel);
            }
            None => {
                reply
                    .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                    .await
            }
        }
        Ok(())
    }
}

// ---- transport -------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

async fn tcp(host: &str, port: u16) -> Result<TcpStream, Failure> {
    let s = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Failure::retry(format!("connection to {host}:{port} timed out")))?
        .map_err(|e| Failure::retry(format!("{host}:{port}: {e}")))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

async fn first_stream(opts: &Options, host: &str, port: u16) -> Result<Box<dyn Io>, Failure> {
    match &opts.proxy {
        None => Ok(Box::new(tcp(host, port).await?)),
        Some(Proxy::Socks5 { host: ph, port: pp }) => {
            let mut s = tcp(ph, *pp).await?;
            crate::proxy::socks5(&mut s, host, port).await.map_err(Failure::retry)?;
            Ok(Box::new(s))
        }
        Some(Proxy::Http { host: ph, port: pp }) => {
            let mut s = tcp(ph, *pp).await?;
            crate::proxy::http_connect(&mut s, host, port)
                .await
                .map_err(Failure::retry)?;
            Ok(Box::new(s))
        }
    }
}

fn config(opts: &Options) -> Arc<client::Config> {
    let mut preferred = Preferred::default();
    if opts.legacy {
        let mut k = preferred.kex.to_vec();
        k.extend([kex::DH_G14_SHA1, kex::DH_GEX_SHA1, kex::DH_G1_SHA1]);
        preferred.kex = k.into();
        let mut c = preferred.cipher.to_vec();
        c.extend([
            cipher::AES_256_CBC,
            cipher::AES_192_CBC,
            cipher::AES_128_CBC,
            cipher::TRIPLE_DES_CBC,
        ]);
        preferred.cipher = c.into();
        let mut m = preferred.mac.to_vec();
        m.extend([mac::HMAC_SHA1_ETM, mac::HMAC_SHA1]);
        preferred.mac = m.into();
    }
    Arc::new(client::Config {
        preferred,
        keepalive_interval: opts.keepalive.then_some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    })
}

pub type SshHandle = Arc<client::Handle<Client>>;

/// Connects through all jump hosts to the target and authenticates every hop.
/// Returns the target's handle and host key type.
pub async fn connect(opts: &Options, ctx: &Ctx) -> Result<(SshHandle, String), Failure> {
    connect_with(opts, ctx, None).await
}

/// [`connect`], delivering remote-forwarded channels of the target to `forwarded`.
pub async fn connect_with(
    opts: &Options,
    ctx: &Ctx,
    forwarded: Option<mpsc::UnboundedSender<russh::Channel<client::Msg>>>,
) -> Result<(SshHandle, String), Failure> {
    let cfg = config(opts);
    let hops: Vec<&Hop> = opts.jumps.iter().chain(std::iter::once(&opts.target)).collect();
    let mut prev: Option<SshHandle> = None;
    let key_type = Arc::new(Mutex::new(String::new()));
    for (i, hop) in hops.iter().enumerate() {
        let stream: Box<dyn Io> = match &prev {
            None => first_stream(opts, &hop.host, hop.port).await?,
            Some(h) => {
                let ch = h
                    .channel_open_direct_tcpip(hop.host.clone(), hop.port as u32, "127.0.0.1", 0)
                    .await
                    .map_err(|e| Failure::retry(format!("jump to {}:{}: {e}", hop.host, hop.port)))?;
                Box::new(ch.into_stream())
            }
        };
        let handler = Client {
            host: hop.host.clone(),
            port: hop.port,
            key_type: key_type.clone(),
            known_hosts: opts.known_hosts.clone(),
            ctx: ctx.clone(),
            agent_forward: opts.agent_forward && i == hops.len() - 1,
            forwarded: forwarded.clone().filter(|_| i == hops.len() - 1),
            x11: opts
                .x11
                .as_ref()
                .filter(|_| i == hops.len() - 1)
                .map(|(d, c)| (crate::x11::display(d), c.clone())),
        };
        let mut handle = client::connect_stream(cfg.clone(), stream, handler)
            .await
            .map_err(|e| match e {
                russh::Error::UnknownKey => Failure::fatal(format!("host key of {} was not accepted", hop.host)),
                e => Failure::retry(format!("{}:{}: {e}", hop.host, hop.port)),
            })?;
        authenticate(&mut handle, hop, ctx).await?;
        prev = Some(Arc::new(handle));
    }
    let kt = key_type.lock().map(|t| t.clone()).unwrap_or_default();
    prev.map(|h| (h, kt)).ok_or_else(|| Failure::fatal("no host"))
}

fn methods(r: &AuthResult) -> Option<russh::MethodSet> {
    match r {
        AuthResult::Success => None,
        AuthResult::Failure { remaining_methods, .. } => Some(remaining_methods.clone()),
    }
}

fn key_with_hash(key: Arc<PrivateKey>, rsa: Option<Option<HashAlg>>) -> PrivateKeyWithHashAlg {
    let hash = if key.algorithm().is_rsa() {
        rsa.flatten().or(Some(HashAlg::Sha256))
    } else {
        None
    };
    PrivateKeyWithHashAlg::new(key, hash)
}

pub async fn load_key(path: &str, hop: &Hop, ctx: &Ctx) -> Result<PrivateKey, Failure> {
    // `vault:<id>` = a key stored in the vault (identities).
    let (full, text) = match path.strip_prefix("vault:") {
        Some(id) => match ctx.secret(id.to_owned()).await {
            Some(t) => (hop.label.clone(), t.to_string()),
            None => {
                return Err(Failure::fatal(format!(
                    "{}: key not available (vault locked?)",
                    hop.label
                )));
            }
        },
        None => {
            let full = crate::sshconfig::expand_home(path);
            let text = tokio::fs::read_to_string(&full)
                .await
                .map_err(|e| Failure::fatal(format!("{full}: {e}")))?;
            (full, text)
        }
    };
    match russh::keys::decode_secret_key(&text, None) {
        Ok(k) => return Ok(k),
        Err(russh::keys::Error::KeyIsEncrypted) => {}
        Err(e) => return Err(Failure::fatal(format!("{full}: {e}"))),
    }
    let vault_id = format!("{}:passphrase", hop.secret_id);
    if let Some(pass) = ctx.secret(vault_id.clone()).await
        && let Ok(k) = russh::keys::decode_secret_key(&text, Some(&pass))
    {
        return Ok(k);
    }
    for _ in 0..3 {
        let prompt = full.clone();
        let Some((pass, save)) = ctx
            .request(|reply| Ask::Password {
                title: hop.label.clone(),
                kind: PromptKind::Passphrase(prompt),
                reply,
            })
            .await
            .flatten()
        else {
            return Err(Failure::fatal("cancelled"));
        };
        if let Ok(k) = russh::keys::decode_secret_key(&text, Some(&pass)) {
            if save {
                ctx.send(Ask::StoreSecret {
                    id: vault_id,
                    value: pass,
                });
            }
            return Ok(k);
        }
    }
    Err(Failure::fatal(format!("{full}: wrong passphrase")))
}

async fn authenticate(handle: &mut client::Handle<Client>, hop: &Hop, ctx: &Ctx) -> Result<(), Failure> {
    let net = |e: russh::Error| Failure::retry(e.to_string());
    let user = hop.user.clone();
    let rsa = handle.best_supported_rsa_hash().await.ok().flatten();
    let first = handle.authenticate_none(user.clone()).await.map_err(net)?;
    let Some(mut allowed) = methods(&first) else {
        return Ok(());
    };
    let has = |m: &russh::MethodSet, kind: russh::MethodKind| m.contains(&kind);

    // 1. Public keys: the chosen key file, then built-in agent, then system agent.
    if has(&allowed, russh::MethodKind::PublicKey) {
        if hop.auth == AuthMethod::Key
            && let Some(path) = &hop.key_path
        {
            let key = Arc::new(load_key(path, hop, ctx).await?);
            let r = handle
                .authenticate_publickey(user.clone(), key_with_hash(key, rsa))
                .await
                .map_err(net)?;
            match methods(&r) {
                None => return Ok(()),
                Some(m) => allowed = m,
            }
        }
        if hop.agent == AgentKind::BuiltIn || hop.auth == AuthMethod::Agent {
            let keys: Vec<Arc<PrivateKey>> = ctx.agent_keys.lock().map(|k| k.clone()).unwrap_or_default();
            for key in keys {
                let r = handle
                    .authenticate_publickey(user.clone(), key_with_hash(key, rsa))
                    .await
                    .map_err(net)?;
                match methods(&r) {
                    None => return Ok(()),
                    Some(m) => allowed = m,
                }
            }
        }
        if hop.agent == AgentKind::System
            && matches!(hop.auth, AuthMethod::Agent | AuthMethod::Key)
            && let Some(()) = system_agent_auth(handle, &user, rsa, &mut allowed).await
        {
            return Ok(());
        }
    }

    // 2. Password (vault, then prompt).
    if has(&allowed, russh::MethodKind::Password) && hop.auth != AuthMethod::KeyboardInteractive {
        let vault_id = format!("{}:password", hop.secret_id);
        if let Some(pw) = ctx.secret(vault_id.clone()).await {
            let r = handle
                .authenticate_password(user.clone(), pw.as_str())
                .await
                .map_err(net)?;
            match methods(&r) {
                None => return Ok(()),
                Some(m) => allowed = m,
            }
        }
        for _ in 0..3 {
            if !has(&allowed, russh::MethodKind::Password) {
                break;
            }
            let Some((pw, save)) = ctx
                .request(|reply| Ask::Password {
                    title: hop.label.clone(),
                    kind: PromptKind::Password(format!("{user}@{}", hop.host)),
                    reply,
                })
                .await
                .flatten()
            else {
                return Err(Failure::fatal("cancelled"));
            };
            let r = handle
                .authenticate_password(user.clone(), pw.as_str())
                .await
                .map_err(net)?;
            match methods(&r) {
                None => {
                    if save {
                        ctx.send(Ask::StoreSecret {
                            id: vault_id,
                            value: pw,
                        });
                    }
                    return Ok(());
                }
                Some(m) => allowed = m,
            }
        }
    }

    // 3. Keyboard-interactive (OTP / 2FA).
    if has(&allowed, russh::MethodKind::KeyboardInteractive) {
        let mut resp = handle
            .authenticate_keyboard_interactive_start(user.clone(), None::<String>)
            .await
            .map_err(net)?;
        loop {
            match resp {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => break,
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    let answers = if prompts.is_empty() {
                        vec![]
                    } else {
                        let prompts = prompts.into_iter().map(|p| (p.prompt, p.echo)).collect();
                        let title = if name.is_empty() { hop.label.clone() } else { name };
                        let Some(a) = ctx
                            .request(|reply| Ask::KeyboardInteractive {
                                title,
                                instructions,
                                prompts,
                                reply,
                            })
                            .await
                            .flatten()
                        else {
                            return Err(Failure::fatal("cancelled"));
                        };
                        a.into_iter().map(|s| s.as_str().to_owned()).collect()
                    };
                    resp = handle
                        .authenticate_keyboard_interactive_respond(answers)
                        .await
                        .map_err(net)?;
                }
            }
        }
    }
    Err(Failure::fatal(format!("authentication failed for {user}@{}", hop.host)))
}

#[cfg(unix)]
async fn system_agent_auth(
    handle: &mut client::Handle<Client>,
    user: &str,
    rsa: Option<Option<HashAlg>>,
    allowed: &mut russh::MethodSet,
) -> Option<()> {
    let mut agent = AgentClient::connect_env().await.ok()?;
    let ids = agent.request_identities().await.ok()?;
    for id in ids {
        let key = id.public_key().into_owned();
        let hash = if key.algorithm().is_rsa() {
            rsa.flatten().or(Some(HashAlg::Sha256))
        } else {
            None
        };
        match handle.authenticate_publickey_with(user, key, hash, &mut agent).await {
            Ok(AuthResult::Success) => return Some(()),
            Ok(AuthResult::Failure { remaining_methods, .. }) => *allowed = remaining_methods,
            Err(e) => tracing::debug!("agent auth: {e:?}"),
        }
    }
    None
}

#[cfg(windows)]
async fn system_agent_auth(
    handle: &mut client::Handle<Client>,
    user: &str,
    rsa: Option<Option<HashAlg>>,
    allowed: &mut russh::MethodSet,
) -> Option<()> {
    let agent = match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(a) => a.dynamic(),
        Err(_) => AgentClient::connect_pageant().await.ok()?.dynamic(),
    };
    let mut agent = agent;
    let ids = agent.request_identities().await.ok()?;
    for id in ids {
        let key = id.public_key().into_owned();
        let hash = if key.algorithm().is_rsa() {
            rsa.flatten().or(Some(HashAlg::Sha256))
        } else {
            None
        };
        match handle.authenticate_publickey_with(user, key, hash, &mut agent).await {
            Ok(AuthResult::Success) => return Some(()),
            Ok(AuthResult::Failure { remaining_methods, .. }) => *allowed = remaining_methods,
            Err(e) => tracing::debug!("agent auth: {e:?}"),
        }
    }
    None
}

// ---- interactive session -------------------------------------------------

enum Ended {
    /// User closed the tab.
    Closed,
    /// Remote shell exited normally.
    Exited(String),
}

/// Shares the live connection with SFTP / monitor (later phases).
pub type Live = Arc<Mutex<Option<SshHandle>>>;

async fn shell(
    handle: &SshHandle,
    opts: &Options,
    wire: &Wire,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    size: &mut Size,
) -> Result<Ended, Failure> {
    let net = |e: russh::Error| Failure::retry(e.to_string());
    let mut ch = handle.channel_open_session().await.map_err(net)?;
    if opts.agent_forward {
        let _ = ch.agent_forward(false).await;
    }
    ch.request_pty(
        false,
        "xterm-256color",
        size.cols.into(),
        size.rows.into(),
        u32::from(size.cols) * u32::from(size.cell_w),
        u32::from(size.rows) * u32::from(size.cell_h),
        &[],
    )
    .await
    .map_err(net)?;
    if let Some((_, cookie)) = &opts.x11 {
        let _ = ch
            .request_x11(false, false, "MIT-MAGIC-COOKIE-1", cookie.as_str(), 0)
            .await;
    }
    let _ = ch.set_env(false, "COLORTERM", "truecolor").await;
    // Like OpenSSH's `SendEnv LANG`: without it the remote shell often runs in
    // the C locale and `ls` escapes non-ASCII names. The reply tells us whether
    // sshd accepted it (`AcceptEnv`); replies come in request order.
    let mut replies: std::collections::VecDeque<Option<String>> = Default::default();
    let lang = match opts.lang.as_deref() {
        Some("") => auto_lang(handle).await,
        other => other.map(str::to_owned),
    };
    if let Some(lang) = &lang {
        ch.set_env(true, "LANG", lang.as_str()).await.map_err(net)?;
        replies.push_back(Some(lang.clone()));
    }
    ch.request_shell(true).await.map_err(net)?;
    replies.push_back(None);
    wire.event(SessionEvent::Connected);
    if let Some(cmd) = opts.after_login.as_deref().filter(|c| !c.trim().is_empty()) {
        ch.data_bytes(format!("{cmd}\r").into_bytes()).await.map_err(net)?;
    }
    let mut exit = None;
    loop {
        tokio::select! {
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data }) => wire.feed(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => wire.feed(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => exit = Some(format!("exit status {exit_status}")),
                Some(ChannelMsg::Success) => {
                    replies.pop_front();
                }
                Some(ChannelMsg::Failure) => {
                    if let Some(Some(lang)) = replies.pop_front() {
                        wire.notice(&opts.notices.lang_refused.replace("{lang}", &lang));
                    }
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => exit = Some(format!("signal {signal_name:?}")),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    return match exit {
                        Some(e) => Ok(Ended::Exited(e)),
                        None if handle.is_closed() => Err(Failure::retry("connection lost")),
                        None => Ok(Ended::Exited("channel closed".into())),
                    };
                }
                _ => {}
            },
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Input(bytes)) => ch.data_bytes(bytes).await.map_err(net)?,
                Some(Cmd::Resize(s)) => {
                    *size = s;
                    let _ = ch.window_change(s.cols.into(), s.rows.into(), 0, 0).await;
                }
                Some(Cmd::Break) => {}
                Some(Cmd::Close) | None => {
                    let _ = ch.close().await;
                    let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
                    return Ok(Ended::Closed);
                }
            },
        }
    }
}

/// `en_US.UTF-8` / `en_US.utf8` → `en_us.utf8`, for comparing locale names.
fn norm_locale(l: &str) -> String {
    l.trim().to_ascii_lowercase().replace('-', "")
}

/// Auto `LANG`: the local UTF-8 `LANG` if the server has it, else `C.UTF-8`,
/// else any UTF-8 locale it has. `available` is `locale -a` output; `None` =
/// it couldn't be read (musl, Windows…) → `C.UTF-8`, which those accept.
/// Sending a locale the server lacks makes bash warn
/// "setlocale: LC_ALL: cannot change locale".
pub fn pick_lang(local: Option<&str>, available: Option<&str>) -> Option<String> {
    let Some(list) = available else {
        return Some("C.UTF-8".into());
    };
    let have: Vec<&str> = list.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let has = |want: &str| have.iter().any(|l| norm_locale(l) == norm_locale(want));
    if let Some(l) = local.filter(|l| norm_locale(l).ends_with(".utf8") && has(l)) {
        return Some(l.to_owned());
    }
    if has("C.UTF-8") {
        return Some("C.UTF-8".into());
    }
    have.iter()
        .find(|l| norm_locale(l).ends_with(".utf8"))
        .map(|l| l.to_string())
}

async fn auto_lang(handle: &SshHandle) -> Option<String> {
    let local = std::env::var("LANG").ok();
    let list = tokio::time::timeout(Duration::from_secs(5), crate::remote::exec_full(handle, "locale -a"))
        .await
        .ok()
        .and_then(Result::ok)
        .filter(|r| r.status == Some(0) && !r.stdout.trim().is_empty())
        .map(|r| r.stdout);
    pick_lang(local.as_deref(), list.as_deref())
}

/// Runs the session until the user closes it or it fails for good.
/// Reconnects with backoff 1→30 s, at most 10 attempts (SPEC §5.1.9).
pub async fn run(opts: Options, ctx: Ctx, wire: Wire, mut size: Size, live: Live) {
    let mut cmds = wire.commands().await;
    let mut attempt = 0u32;
    let mut connected_once = false;
    loop {
        let result = match connect(&opts, &ctx).await {
            Ok((handle, key_type)) => {
                if connected_once {
                    let time = crate::termlog::local_time().get(11..16).unwrap_or_default().to_owned();
                    wire.notice(&opts.notices.reconnected.replace("{time}", &time));
                }
                connected_once = true;
                attempt = 0;
                let t = &opts.target;
                wire.event(SessionEvent::Info {
                    target: format!("{}@{}:{}", t.user, t.host, t.port),
                    detail: format!("SSH-2 · {key_type}"),
                });
                if let Ok(mut l) = live.lock() {
                    *l = Some(handle.clone());
                }
                let r = shell(&handle, &opts, &wire, &mut cmds, &mut size).await;
                if let Ok(mut l) = live.lock() {
                    *l = None;
                }
                r
            }
            Err(e) => Err(e),
        };
        let failure = match result {
            Ok(Ended::Closed) => return,
            Ok(Ended::Exited(reason)) => {
                wire.event(SessionEvent::Disconnected(reason));
                return;
            }
            Err(f) => f,
        };
        wire.event(SessionEvent::Disconnected(failure.msg.clone()));
        if failure.fatal || !opts.reconnect || !connected_once || attempt >= 10 {
            return;
        }
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
        attempt += 1;
        wire.notice(
            &opts
                .notices
                .retrying
                .replace("{reason}", &failure.msg)
                .replace("{n}", &attempt.to_string())
                .replace("{secs}", &delay.as_secs().to_string()),
        );
        let deadline = tokio::time::sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Resize(s)) => size = s,
                    Some(Cmd::Close) | None => return,
                    Some(Cmd::Input(_) | Cmd::Break) => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pick_lang;

    #[test]
    fn auto_lang() {
        let debian = "C\nC.utf8\nPOSIX\nru_RU.utf8\n";
        // Local en_US.UTF-8 is missing on the server → C.UTF-8, not a bash warning.
        assert_eq!(pick_lang(Some("en_US.UTF-8"), Some(debian)).as_deref(), Some("C.UTF-8"));
        assert_eq!(
            pick_lang(Some("ru_RU.UTF-8"), Some(debian)).as_deref(),
            Some("ru_RU.UTF-8")
        );
        assert_eq!(
            pick_lang(None, Some("C\nPOSIX\nen_GB.utf8\n")).as_deref(),
            Some("en_GB.utf8")
        );
        assert_eq!(pick_lang(None, Some("C\nPOSIX\n")), None);
        assert_eq!(pick_lang(Some("en_US.UTF-8"), None).as_deref(), Some("C.UTF-8"));
    }
}
