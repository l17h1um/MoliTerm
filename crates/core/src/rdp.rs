//! RDP (SPEC §5.8) on IronRDP: NLA (CredSSP/NTLM) or TLS, a shared RGBA frame
//! the UI uploads by dirty rectangle, fast-path input, text clipboard, and
//! Display Control resizes. The desktop types ([`Frame`], [`Input`], [`Link`])
//! are protocol-agnostic so a VNC transport can reuse the tab frame later.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use ironrdp_cliprdr::{
    CliprdrClient,
    backend::CliprdrBackend,
    pdu::{
        ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
        FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
    },
};
use ironrdp_connector::{
    ClientConnector, ClientConnectorState, ConnectionResult, Credentials, DesktopSize, ServerName,
    connection_activation::ConnectionActivationState,
};
use ironrdp_core::WriteBuf;
use ironrdp_displaycontrol::{client::DisplayControlClient, pdu::MonitorLayoutEntry};
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{Database, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::{
    gcc::KeyboardType,
    geometry::InclusiveRectangle,
    nego::SecurityProtocol,
    rdp::{
        capability_sets::MajorPlatformType,
        client_info::{PerformanceFlags, TimezoneInfo},
    },
};
use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput, fast_path, image::DecodedImage};
use ironrdp_tokio::{FramedWrite as _, TokioFramed, single_sequence_step_read};
use sha2::{Digest, Sha256};
use tokio::{
    net::TcpStream,
    sync::{Mutex as AsyncMutex, mpsc as tmpsc},
};
use zeroize::Zeroizing;

use crate::{
    ssh::{Ask, Ctx, HostKeyAnswer, PromptKind},
    terminal::{SessionEvent, Waker},
};

/// RDP settings stored in `Session.extra`; host/port/username are the session's.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Extra {
    pub domain: String,
    /// Fit / 100% / Resize remote, as last chosen in the tab toolbar.
    pub scale: Scale,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    Fit,
    Actual,
    #[default]
    Resize,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub host: String,
    pub port: u16,
    /// `user`, `DOMAIN\user` or `user@domain`.
    pub user: String,
    pub domain: String,
    /// Vault id prefix; the password lives at `<prefix>:password`.
    pub secret_id: String,
    /// TOFU store of server certificate fingerprints.
    pub known_hosts: PathBuf,
    /// Initial desktop size.
    pub size: (u16, u16),
}

/// Desktop pixels shared with the UI: RGBA, `width * height * 4` bytes.
#[derive(Default)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub rgba: Vec<u8>,
    /// Union of updated areas since the UI last looked: `[x0, y0, x1, y1]`, exclusive end.
    pub dirty: Option<[u16; 4]>,
    /// Bumped when the size changes (UI recreates its texture).
    pub generation: u64,
}

pub type SharedFrame = Arc<Mutex<Frame>>;

/// UI → session.
#[derive(Debug, Clone)]
pub enum Input {
    /// PC/AT set-1 scancode; `0xE0xx` for extended keys.
    Key {
        code: u16,
        down: bool,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    /// 0 left, 1 middle, 2 right, 3 back, 4 forward.
    Button {
        button: u8,
        down: bool,
    },
    /// Positive = away from the user / right; 120 units per notch.
    Wheel {
        vertical: bool,
        units: i16,
    },
    /// Focus lost: release every pressed key and button.
    ReleaseAll,
    /// Ask the server to change the desktop size (Display Control).
    Resize {
        width: u16,
        height: u16,
    },
    CtrlAltDel,
    Close,
}

/// UI-side end of a desktop session.
pub struct Handle {
    pub frame: SharedFrame,
    pub input: tmpsc::UnboundedSender<Input>,
    pub events: mpsc::Receiver<SessionEvent>,
}

impl Handle {
    pub fn send(&self, i: Input) {
        let _ = self.input.send(i);
    }
}

/// Session-side end, reusable across reconnects of the same tab.
#[derive(Clone)]
pub struct Link {
    frame: SharedFrame,
    events: mpsc::Sender<SessionEvent>,
    input: Arc<AsyncMutex<tmpsc::UnboundedReceiver<Input>>>,
    wake: Waker,
}

impl Link {
    fn event(&self, ev: SessionEvent) {
        let _ = self.events.send(ev);
        (self.wake)();
    }
}

pub fn pipeline(wake: Waker) -> (Handle, Link) {
    let frame: SharedFrame = Default::default();
    let (tx, rx) = tmpsc::unbounded_channel();
    let (ev_tx, events) = mpsc::channel();
    (
        Handle {
            frame: frame.clone(),
            input: tx,
            events,
        },
        Link {
            frame,
            events: ev_tx,
            input: Arc::new(AsyncMutex::new(rx)),
            wake,
        },
    )
}

type Error = Box<dyn std::error::Error + Send + Sync>;

fn err(e: impl std::fmt::Display) -> Error {
    e.to_string().into()
}

/// Runs one connection until it ends; reports the end as `Disconnected`.
pub async fn run(mut opts: Options, ctx: Ctx, link: Link) {
    let mut input = link.input.lock().await;
    while input.try_recv().is_ok() {}
    link.event(SessionEvent::Info {
        target: format!("{}@{}:{}", opts.user, opts.host, opts.port),
        detail: "RDP".into(),
    });
    // Password that worked, kept while this tab reconnects for a resize.
    let mut cached = None;
    let mut prompt = false;
    let reason = loop {
        match session(&opts, &ctx, &link, &mut input, prompt, &mut cached).await {
            Ok(End::Closed) => return,
            Ok(End::Ended(reason)) => break reason,
            // The server can't resize a live session: reconnect at the new size.
            Ok(End::Resize(w, h)) => opts.size = (w, h),
            // A stale vault password: ask once instead.
            Err(Fail::VaultAuth(_)) if !prompt => prompt = true,
            Err(Fail::VaultAuth(e) | Fail::Other(e)) => break e.to_string(),
        }
    };
    link.event(SessionEvent::Disconnected(reason));
}

enum End {
    /// The user closed the tab.
    Closed,
    Ended(String),
    Resize(u16, u16),
}

/// `DOMAIN\user` / `user@domain` → (user, domain); the explicit domain field wins.
fn split_user(user: &str, domain: &str) -> (String, Option<String>) {
    let (u, d) = match (user.split_once('\\'), user.split_once('@')) {
        (Some((d, u)), _) => (u, Some(d)),
        (None, Some((u, d))) => (u, Some(d)),
        _ => (user, None),
    };
    let d = Some(domain.trim()).filter(|d| !d.is_empty()).or(d);
    (u.to_owned(), d.map(str::to_owned))
}

enum Fail {
    /// Authentication failed with the password from the vault.
    VaultAuth(Error),
    Other(Error),
}

impl<E: Into<Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail::Other(e.into())
    }
}

/// Password and whether it came from the vault, or was typed with "save" ticked
/// (`Some(id)`: store after a successful login).
async fn password(opts: &Options, ctx: &Ctx, prompt: bool) -> Option<(Zeroizing<String>, bool, Option<String>)> {
    let id = format!("{}:password", opts.secret_id);
    if !prompt && let Some(pw) = ctx.secret(id.clone()).await {
        return Some((pw, true, None));
    }
    let (pw, save) = ctx
        .request(|reply| Ask::Password {
            title: opts.host.clone(),
            kind: PromptKind::Password(format!("{}@{}", opts.user, opts.host)),
            reply,
        })
        .await
        .flatten()?;
    Some((pw, false, save.then_some(id)))
}

fn config(opts: &Options, username: String, password: String, domain: Option<String>) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        // Graphical login (TLS without NLA) is allowed only when the server
        // insists on it; the tab then shows a warning.
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        // US: we send layout-independent scancodes; the remote side's own
        // input language decides what they mean.
        keyboard_layout: 0x409,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: opts.size.0,
            height: opts.size.1,
        },
        bitmap: None,
        client_build: 0,
        client_name: "MoliTerm".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(not(any(windows, target_os = "macos")))]
        platform: MajorPlatformType::UNIX,
        // The local OS cursor is shown over the desktop.
        enable_server_pointer: false,
        request_data: None,
        // Use the credentials for the logon itself (TLS-only servers like xrdp
        // would otherwise show their own login screen).
        autologon: true,
        enable_audio_playback: false,
        // ponytail: no bulk compression, so reactivation needs no decompressor state; add Rdp61 if WAN bandwidth matters.
        compression_type: None,
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// No Kerberos: CredSSP with NTLM never asks for network requests.
struct NoNetwork;

impl ironrdp_tokio::NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!("Kerberos is not supported"))
    }
}

pub fn fingerprint(public_key: &[u8]) -> String {
    use base64::Engine as _;
    let hash = Sha256::digest(public_key);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash)
    )
}

/// Trust on first use, like SSH host keys: `host:port SHA256:…` per line.
async fn check_certificate(opts: &Options, ctx: &Ctx, public_key: &[u8]) -> Result<(), Error> {
    let fp = fingerprint(public_key);
    let id = format!("{}:{}", opts.host.to_lowercase(), opts.port);
    let text = std::fs::read_to_string(&opts.known_hosts).unwrap_or_default();
    let saved = text.lines().find_map(|l| {
        l.split_once(' ')
            .filter(|(h, _)| *h == id)
            .map(|(_, f)| f.trim().to_owned())
    });
    if saved.as_deref() == Some(fp.as_str()) {
        return Ok(());
    }
    let answer = ctx
        .request(|reply| Ask::HostKey {
            host: opts.host.clone(),
            port: opts.port,
            key_type: "RDP TLS".into(),
            fingerprint: fp.clone(),
            changed: saved.clone(),
            reply,
        })
        .await
        .unwrap_or(HostKeyAnswer::Reject);
    match answer {
        HostKeyAnswer::Reject => Err(err("server certificate was not accepted")),
        HostKeyAnswer::Once => Ok(()),
        HostKeyAnswer::Save => {
            let mut lines: Vec<String> = text
                .lines()
                .filter(|l| !l.starts_with(&format!("{id} ")))
                .map(str::to_owned)
                .collect();
            lines.push(format!("{id} {fp}"));
            if let Err(e) = std::fs::write(&opts.known_hosts, lines.join("\n") + "\n") {
                tracing::warn!("rdp known hosts write failed: {e}");
            }
            Ok(())
        }
    }
}

/// Clipboard callbacks → session loop.
#[derive(Debug)]
enum Clip {
    /// Send our format list (on init and when asked).
    Announce,
    RemoteHasText,
    /// The server wants our text.
    Request,
    Text(String),
}

#[derive(Debug)]
struct ClipBackend(tmpsc::UnboundedSender<Clip>);

ironrdp_core::impl_as_any!(ClipBackend);

impl CliprdrBackend for ClipBackend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }
    fn on_ready(&mut self) {}
    fn on_request_format_list(&mut self) {
        let _ = self.0.send(Clip::Announce);
    }
    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}
    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        if formats.iter().any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT) {
            let _ = self.0.send(Clip::RemoteHasText);
        }
    }
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let _ = self.0.send(Clip::Request);
        }
    }
    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if !response.is_error()
            && let Ok(text) = response.to_unicode_string()
        {
            let _ = self.0.send(Clip::Text(text));
        }
    }
    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}

/// Local clipboard (text). `None` when the OS clipboard is unavailable.
struct Local {
    board: Option<arboard::Clipboard>,
    /// Last text seen or written, to detect local changes.
    last: String,
}

impl Local {
    fn get(&mut self) -> Option<String> {
        self.board.as_mut()?.get_text().ok()
    }
    fn set(&mut self, text: &str) {
        self.last = text.to_owned();
        if let Some(b) = &mut self.board {
            let _ = b.set_text(text.to_owned());
        }
    }
}

async fn session(
    opts: &Options,
    ctx: &Ctx,
    link: &Link,
    input: &mut tmpsc::UnboundedReceiver<Input>,
    prompt: bool,
    cached: &mut Option<Zeroizing<String>>,
) -> Result<End, Fail> {
    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((opts.host.as_str(), opts.port)),
    )
    .await
    .map_err(|_| err(format!("connection to {}:{} timed out", opts.host, opts.port)))?
    .map_err(|e| err(format!("{}:{}: {e}", opts.host, opts.port)))?;
    let client_addr = tcp.local_addr()?;
    let _ = tcp.set_nodelay(true);
    let (password, from_vault, save) = match cached.clone() {
        Some(pw) => (pw, false, None),
        None => match password(opts, ctx, prompt).await {
            Some(p) => p,
            None => return Err(err("cancelled").into()),
        },
    };
    let (user, domain) = split_user(&opts.user, &opts.domain);
    let (clip_tx, mut clip_rx) = tmpsc::unbounded_channel();
    let mut connector = ClientConnector::new(config(opts, user, password.to_string(), domain), client_addr)
        .with_static_channel(DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new()))))
        .with_static_channel(CliprdrClient::new(Box::new(ClipBackend(clip_tx))));

    let mut framed = TokioFramed::new(tcp);
    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(err)?;
    let weak = match &connector.state {
        ClientConnectorState::EnhancedSecurityUpgrade { selected_protocol } => {
            !selected_protocol.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX)
        }
        _ => false,
    };
    let (tls, cert) = ironrdp_tls::upgrade(framed.into_inner_no_leftover(), &opts.host).await?;
    let public_key = ironrdp_tls::extract_tls_server_public_key(&cert)
        .ok_or_else(|| err("server certificate has no public key"))?
        .to_vec();
    check_certificate(opts, ctx, &public_key).await?;
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new(tls);
    let result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ServerName::new(opts.host.clone()),
        public_key,
        None,
    )
    .await
    .map_err(|e| {
        let credssp = matches!(e.kind(), ironrdp_connector::ConnectorErrorKind::Credssp(_));
        let e = err(format!("{e} ({})", e.kind()));
        if credssp && from_vault {
            Fail::VaultAuth(e)
        } else {
            Fail::Other(e)
        }
    })?;
    if let Some(id) = save {
        ctx.send(Ask::StoreSecret {
            id,
            value: password.clone(),
        });
    }
    *cached = Some(password);

    link.event(SessionEvent::Connected);
    if weak {
        link.event(SessionEvent::WeakSecurity);
    }
    Ok(active(result, framed, link, input, &mut clip_rx).await?)
}

/// Copies the updated rectangle of `image` into the shared frame.
fn publish(link: &Link, image: &DecodedImage, rect: Option<&InclusiveRectangle>) {
    let Ok(mut f) = link.frame.lock() else {
        return;
    };
    let (w, h) = (image.width(), image.height());
    if f.width != w || f.height != h {
        f.width = w;
        f.height = h;
        f.rgba = vec![0; usize::from(w) * usize::from(h) * 4];
        f.generation += 1;
        f.dirty = None;
    }
    let (x0, y0, x1, y1) = match rect {
        Some(r) => (r.left.min(w), r.top.min(h), (r.right + 1).min(w), (r.bottom + 1).min(h)),
        None => (0, 0, w, h),
    };
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let stride = image.stride();
    let data = image.data();
    for y in usize::from(y0)..usize::from(y1) {
        let a = y * stride + usize::from(x0) * 4;
        let b = y * stride + usize::from(x1) * 4;
        let dst = &mut f.rgba[a..b];
        dst.copy_from_slice(&data[a..b]);
        // Decoders leave alpha unset; the frame is drawn opaque.
        for px in dst.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    f.dirty = Some(match f.dirty {
        Some([a, b, c, d]) => [a.min(x0), b.min(y0), c.max(x1), d.max(y1)],
        None => [x0, y0, x1, y1],
    });
    drop(f);
    (link.wake)();
}

async fn active<S>(
    result: ConnectionResult,
    mut framed: TokioFramed<S>,
    link: &Link,
    input: &mut tmpsc::UnboundedReceiver<Input>,
    clip_rx: &mut tmpsc::UnboundedReceiver<Clip>,
) -> Result<End, Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync,
{
    let factory = result.activation_factory;
    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    publish(link, &image, None);
    let mut stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut db = Database::new();
    let mut local = Local {
        board: arboard::Clipboard::new().ok(),
        last: String::new(),
    };
    local.last = local.get().unwrap_or_default();
    let mut clip_ready = false;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    // A resize we asked for: (width, height, when).
    let mut resizing: Option<(u16, u16, std::time::Instant)> = None;

    loop {
        let mut frames: Vec<Vec<u8>> = vec![];
        tokio::select! {
            pdu = framed.read_pdu() => {
                let (action, payload) = pdu?;
                for out in stage.process(&mut image, action, &payload).map_err(err)? {
                    match out {
                        ActiveStageOutput::ResponseFrame(f) => frames.push(f),
                        ActiveStageOutput::GraphicsUpdate(r) => publish(link, &image, Some(&r)),
                        ActiveStageOutput::Terminate(reason) => return Ok(End::Ended(reason.description())),
                        ActiveStageOutput::DeactivateAll => {
                            // Deactivation-Reactivation (e.g. after a resize): new size, new share.
                            let mut seq = factory.create();
                            let mut buf = WriteBuf::new();
                            loop {
                                let written = single_sequence_step_read(&mut framed, &mut seq, &mut buf)
                                    .await
                                    .map_err(err)?;
                                if written.size().is_some() {
                                    framed.write_all(buf.filled()).await?;
                                }
                                if let ConnectionActivationState::Finalized {
                                    desktop_size,
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                } = seq.connection_activation_state()
                                {
                                    image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
                                    stage.set_fastpath_processor(
                                        fast_path::ProcessorBuilder {
                                            io_channel_id: seq.io_channel_id(),
                                            user_channel_id: seq.user_channel_id(),
                                            share_id,
                                            enable_server_pointer,
                                            pointer_software_rendering,
                                            bulk_decompressor: None,
                                        }
                                        .build(),
                                    );
                                    stage.set_share_id(share_id);
                                    stage.set_enable_server_pointer(enable_server_pointer);
                                    publish(link, &image, None);
                                    resizing = None;
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            i = input.recv() => {
                let ops: Vec<Operation> = match i {
                    None | Some(Input::Close) => {
                        if let Ok(out) = stage.graceful_shutdown() {
                            for o in out {
                                if let ActiveStageOutput::ResponseFrame(f) = o {
                                    let _ = framed.write_all(&f).await;
                                }
                            }
                        }
                        return Ok(End::Closed);
                    }
                    Some(Input::Key { code, down }) => {
                        let sc = Scancode::from_u16(code);
                        vec![if down { Operation::KeyPressed(sc) } else { Operation::KeyReleased(sc) }]
                    }
                    Some(Input::MouseMove { x, y }) => vec![Operation::MouseMove(MousePosition { x, y })],
                    Some(Input::Button { button, down }) => match ironrdp_input::MouseButton::from_idx(button.into()) {
                        Some(b) if down => vec![Operation::MouseButtonPressed(b)],
                        Some(b) => vec![Operation::MouseButtonReleased(b)],
                        None => vec![],
                    },
                    Some(Input::Wheel { vertical, units }) => vec![Operation::WheelRotations(WheelRotations {
                        is_vertical: vertical,
                        rotation_units: units,
                    })],
                    Some(Input::ReleaseAll) => {
                        let events = db.release_all();
                        for o in stage.process_fastpath_input(&mut image, &events).map_err(err)? {
                            if let ActiveStageOutput::ResponseFrame(f) = o {
                                frames.push(f);
                            }
                        }
                        vec![]
                    }
                    Some(Input::CtrlAltDel) => {
                        let (ctrl, alt, del) = (Scancode::from_u16(0x1d), Scancode::from_u16(0x38), Scancode::from_u16(0xe053));
                        vec![
                            Operation::KeyPressed(ctrl),
                            Operation::KeyPressed(alt),
                            Operation::KeyPressed(del),
                            Operation::KeyReleased(del),
                            Operation::KeyReleased(alt),
                            Operation::KeyReleased(ctrl),
                        ]
                    }
                    Some(Input::Resize { width, height }) => {
                        let (w, h) = MonitorLayoutEntry::adjust_display_size(width.into(), height.into());
                        let (w16, h16) = (w as u16, h as u16);
                        match stage.encode_resize(w, h, None, None) {
                            Some(r) => {
                                frames.push(r.map_err(err)?);
                                resizing = Some((w16, h16, std::time::Instant::now()));
                            }
                            None => return Ok(End::Resize(w16, h16)),
                        }
                        vec![]
                    }
                };
                if !ops.is_empty() {
                    let events = db.apply(ops);
                    for o in stage.process_fastpath_input(&mut image, &events).map_err(err)? {
                        match o {
                            ActiveStageOutput::ResponseFrame(f) => frames.push(f),
                            ActiveStageOutput::GraphicsUpdate(r) => publish(link, &image, Some(&r)),
                            _ => {}
                        }
                    }
                }
            }
            Some(c) = clip_rx.recv() => {
                let Some(cliprdr) = stage.get_svc_processor_mut::<CliprdrClient>() else { continue };
                let msgs = match c {
                    Clip::Announce => {
                        clip_ready = true;
                        let formats = if local.get().is_some_and(|t| !t.is_empty()) {
                            vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
                        } else {
                            vec![]
                        };
                        cliprdr.initiate_copy(&formats).ok()
                    }
                    Clip::RemoteHasText => cliprdr.initiate_paste(ClipboardFormatId::CF_UNICODETEXT).ok(),
                    Clip::Request => {
                        let text = local.get().unwrap_or_default();
                        cliprdr.submit_format_data(OwnedFormatDataResponse::new_unicode_string(&text)).ok()
                    }
                    Clip::Text(text) => {
                        local.set(&text.replace("\r\n", "\n"));
                        None
                    }
                };
                if let Some(m) = msgs {
                    frames.push(stage.process_svc_processor_messages(m).map_err(err)?);
                }
            }
            _ = poll.tick() => {
                if let Some((w, h, at)) = resizing
                    && at.elapsed() > Duration::from_secs(2)
                {
                    if (image.width(), image.height()) == (w, h) {
                        resizing = None;
                    } else {
                        // Display Control ignored (e.g. xrdp 0.9): reconnect instead.
                        return Ok(End::Resize(w, h));
                    }
                }
                // Local copy → offer it to the server.
                if clip_ready
                    && let Some(text) = local.get()
                    && text != local.last
                {
                    local.last = text;
                    if let Some(cliprdr) = stage.get_svc_processor_mut::<CliprdrClient>()
                        && let Ok(m) = cliprdr.initiate_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)])
                    {
                        frames.push(stage.process_svc_processor_messages(m).map_err(err)?);
                    }
                }
            }
        }
        for f in frames.iter().filter(|f| !f.is_empty()) {
            framed.write_all(f).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_and_fingerprints() {
        assert_eq!(split_user("CORP\\bob", ""), ("bob".into(), Some("CORP".into())));
        assert_eq!(
            split_user("bob@corp.local", ""),
            ("bob".into(), Some("corp.local".into()))
        );
        assert_eq!(split_user("bob", " LAB "), ("bob".into(), Some("LAB".into())));
        assert_eq!(split_user("CORP\\bob", "LAB"), ("bob".into(), Some("LAB".into())));
        assert_eq!(split_user("bob", ""), ("bob".into(), None));
        assert!(fingerprint(b"key").starts_with("SHA256:"));
    }
}
