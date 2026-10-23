//! Terminal model shared by all protocols. A session task owns the transport,
//! pushes received bytes through a [`Feeder`] (VT parse under a short lock,
//! OSC 7 scan, logging) and wakes the UI. The UI only locks [`SharedTerm`] to paint.

use std::sync::{Arc, Mutex, RwLock, mpsc};

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line, Point},
    sync::FairMutex,
    term::{Config, Term},
    vte::ansi::{Processor, Rgb as VteRgb},
};
use tokio::sync::{
    Mutex as AsyncMutex,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
    osc7::Osc7,
    termlog::SessionLog,
    theme::{Rgb, TermColors},
};

pub type SharedTerm = Arc<FairMutex<Term<Listener>>>;
pub type Waker = Arc<dyn Fn() + Send + Sync>;
pub type Encoding = &'static encoding_rs::Encoding;

/// Session encoding by label ("Windows-1251", "KOI8-R", "CP866", …);
/// `None` for UTF-8 and unknown labels.
pub fn encoding(label: Option<&str>) -> Option<Encoding> {
    let e = encoding_rs::Encoding::for_label(label?.trim().as_bytes())?;
    (e != encoding_rs::UTF_8).then_some(e)
}

/// UTF-8 input → session encoding; unmappable characters become `?`.
pub fn encode(enc: Option<Encoding>, bytes: Vec<u8>) -> Vec<u8> {
    let Some(enc) = enc else {
        return bytes;
    };
    let s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => return e.into_bytes(),
    };
    if s.is_ascii() {
        return s.into_bytes();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let (bytes, _, bad) = enc.encode(ch.encode_utf8(&mut buf));
        if bad {
            out.push(b'?');
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    out
}

/// UI → session.
#[derive(Debug)]
pub enum Cmd {
    Input(Vec<u8>),
    Resize(Size),
    /// Serial BREAK; other transports ignore it.
    Break,
    Close,
}

/// Session → UI.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Title(Option<String>),
    /// OSC 52 clipboard write request.
    Clipboard(String),
    Bell,
    Cwd(String),
    Connected,
    /// Status bar text: `user@host:port` and e.g. `SSH-2`.
    Info {
        target: String,
        detail: String,
    },
    /// Technical reason, shown under a localized headline.
    Disconnected(String),
    /// RDP fell back to TLS without NLA (graphical logon).
    WeakSecurity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

#[derive(Clone)]
pub struct Listener {
    events: mpsc::Sender<SessionEvent>,
    input: UnboundedSender<Cmd>,
    colors: Arc<RwLock<TermColors>>,
    wake: Waker,
}

impl Listener {
    fn color(&self, index: usize) -> Rgb {
        let Ok(c) = self.colors.read() else {
            return Rgb(0, 0, 0);
        };
        match index {
            0..=255 => indexed_color(index as u8, &c.ansi),
            257 => c.bg,
            258 => c.cursor,
            _ => c.fg,
        }
    }
}

/// xterm 256-color palette: 0–15 from the theme, then the 6×6×6 cube and greys.
pub fn indexed_color(i: u8, ansi: &[Rgb; 16]) -> Rgb {
    match i {
        0..=15 => ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let v = |n: u8| if n == 0 { 0 } else { n * 40 + 55 };
            Rgb(v(i / 36), v(i / 6 % 6), v(i % 6))
        }
        _ => {
            let g = (i - 232) * 10 + 8;
            Rgb(g, g, g)
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let ev = match event {
            Event::PtyWrite(s) => {
                let _ = self.input.send(Cmd::Input(s.into_bytes()));
                return;
            }
            Event::ColorRequest(i, reply) => {
                let Rgb(r, g, b) = self.color(i);
                let _ = self.input.send(Cmd::Input(reply(VteRgb { r, g, b }).into_bytes()));
                return;
            }
            Event::Title(t) => SessionEvent::Title(Some(t)),
            Event::ResetTitle => SessionEvent::Title(None),
            Event::ClipboardStore(_, text) => SessionEvent::Clipboard(text),
            Event::Bell => SessionEvent::Bell,
            _ => return,
        };
        let _ = self.events.send(ev);
        (self.wake)();
    }
}

/// Session-side end: turns transport bytes into terminal state.
pub struct Feeder {
    term: SharedTerm,
    parser: Processor,
    osc7: Osc7,
    log: Option<SessionLog>,
    events: mpsc::Sender<SessionEvent>,
    wake: Waker,
    /// Non-UTF-8 session encoding; keeps state across split chunks.
    decoder: Option<encoding_rs::Decoder>,
}

impl Feeder {
    pub fn feed(&mut self, bytes: &[u8]) {
        let decoded;
        let bytes = match &mut self.decoder {
            Some(d) => {
                let mut s = String::with_capacity(d.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len() * 3));
                let _ = d.decode_to_string(bytes, &mut s, false);
                decoded = s;
                decoded.as_bytes()
            }
            None => bytes,
        };
        self.parser.advance(&mut *self.term.lock(), bytes);
        if let Some(cwd) = self.osc7.feed(bytes) {
            let _ = self.events.send(SessionEvent::Cwd(cwd));
        }
        if let Some(log) = &mut self.log
            && let Err(e) = log.write(bytes)
        {
            tracing::warn!("session log disabled: {e}");
            self.log = None;
        }
        (self.wake)();
    }

    pub fn event(&self, ev: SessionEvent) {
        let _ = self.events.send(ev);
        (self.wake)();
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        if let Some(log) = &mut self.log {
            let _ = log.flush();
        }
    }
}

/// UI-side end of a session.
pub struct Handle {
    pub term: SharedTerm,
    pub cmd: UnboundedSender<Cmd>,
    pub events: mpsc::Receiver<SessionEvent>,
    pub encoding: Option<Encoding>,
}

impl Handle {
    /// Typed/pasted UTF-8 text; converted to the session encoding.
    pub fn send(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.cmd.send(Cmd::Input(encode(self.encoding, bytes.into())));
    }
}

/// Session-side end, cloneable and reusable across reconnects of the same tab.
#[derive(Clone)]
pub struct Wire {
    feeder: Arc<Mutex<Feeder>>,
    cmds: Arc<AsyncMutex<UnboundedReceiver<Cmd>>>,
    tx: UnboundedSender<Cmd>,
}

impl Wire {
    pub fn feed(&self, bytes: &[u8]) {
        if let Ok(mut f) = self.feeder.lock() {
            f.feed(bytes);
        }
    }

    pub fn event(&self, ev: SessionEvent) {
        if let Ok(f) = self.feeder.lock() {
            f.event(ev);
        }
    }

    /// Prints a dim separator line, e.g. `── reconnected 09:41 ──`.
    pub fn notice(&self, text: &str) {
        self.feed(format!("\r\n\x1b[2m── {text} ──\x1b[0m\r\n").as_bytes());
    }

    /// Drops commands left over from a previous run (e.g. a second `Close`).
    /// Call synchronously before starting a new run.
    pub fn drain(&self) {
        if let Ok(mut rx) = self.cmds.try_lock() {
            while rx.try_recv().is_ok() {}
        }
    }

    /// The command queue; hold the guard for the whole transport run.
    pub fn commands_blocking(&self) -> tokio::sync::MutexGuard<'_, UnboundedReceiver<Cmd>> {
        self.cmds.blocking_lock()
    }

    pub async fn commands(&self) -> tokio::sync::MutexGuard<'_, UnboundedReceiver<Cmd>> {
        self.cmds.lock().await
    }

    /// Wakes a transport blocked on the command queue so it can exit.
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

pub fn pipeline(
    size: Size,
    scrollback: usize,
    colors: Arc<RwLock<TermColors>>,
    log: Option<SessionLog>,
    encoding: Option<Encoding>,
    wake: Waker,
) -> (Handle, Wire) {
    let (cmd_tx, cmds) = unbounded_channel();
    let (ev_tx, events) = mpsc::channel();
    let listener = Listener {
        events: ev_tx.clone(),
        input: cmd_tx.clone(),
        colors,
        wake: wake.clone(),
    };
    let config = Config {
        scrolling_history: scrollback,
        ..Default::default()
    };
    let term = Arc::new(FairMutex::new(Term::new(config, &size, listener)));
    let feeder = Feeder {
        term: term.clone(),
        parser: Processor::new(),
        osc7: Osc7::default(),
        log,
        events: ev_tx,
        wake,
        decoder: encoding.map(|e| e.new_decoder_without_bom_handling()),
    };
    let handle = Handle {
        term,
        cmd: cmd_tx.clone(),
        events,
        encoding,
    };
    let wire = Wire {
        feeder: Arc::new(Mutex::new(feeder)),
        cmds: Arc::new(AsyncMutex::new(cmds)),
        tx: cmd_tx,
    };
    (handle, wire)
}

/// Whole buffer (scrollback + screen) as text.
pub fn all_text<T>(term: &Term<T>) -> String {
    let grid = term.grid();
    let start = Point::new(Line(-(grid.history_size() as i32)), Column(0));
    let end = Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column());
    term.bounds_to_string(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Arc<RwLock<TermColors>> {
        let t = crate::theme::load_all(std::path::Path::new("/nonexistent"));
        Arc::new(RwLock::new(t[0].term.clone()))
    }

    #[test]
    fn feed_title_cwd_and_reply() {
        let size = Size {
            cols: 20,
            rows: 5,
            cell_w: 8,
            cell_h: 16,
        };
        let (handle, wire) = pipeline(size, 100, colors(), None, None, Arc::new(|| {}));
        wire.feed(b"hello\r\n\x1b]0;my title\x07\x1b]7;file://h/tmp\x07\x1b[6n");
        let text = all_text(&*handle.term.lock());
        assert!(text.starts_with("hello"));
        let evs: Vec<_> = handle.events.try_iter().collect();
        assert!(evs.contains(&SessionEvent::Title(Some("my title".into()))));
        assert!(evs.contains(&SessionEvent::Cwd("/tmp".into())));
        // Cursor position report goes back to the transport.
        match wire.cmds.blocking_lock().try_recv() {
            Ok(Cmd::Input(b)) => assert_eq!(b, b"\x1b[2;1R"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn session_encodings() {
        let size = Size {
            cols: 20,
            rows: 3,
            cell_w: 8,
            cell_h: 16,
        };
        let enc = encoding(Some("Windows-1251"));
        assert_eq!(encoding(Some("UTF-8")), None);
        for label in ["KOI8-R", "CP866", "ISO-8859-1", "ISO-8859-5"] {
            assert!(encoding(Some(label)).is_some(), "{label}");
        }
        let (handle, wire) = pipeline(size, 100, colors(), None, enc, Arc::new(|| {}));
        // "Привет" in CP1251, split mid-word.
        wire.feed(b"\xcf\xf0\xe8");
        wire.feed(b"\xe2\xe5\xf2");
        assert!(all_text(&*handle.term.lock()).starts_with("Привет"));
        assert_eq!(encode(enc, "да ✓".as_bytes().to_vec()), b"\xe4\xe0 ?");
        assert_eq!(encode(None, "да".as_bytes().to_vec()), "да".as_bytes());
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p moliterm-core throughput -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn throughput() {
        let colors = Arc::new(RwLock::new(
            crate::theme::load_all(std::path::Path::new("/x"))[0].term.clone(),
        ));
        let size = Size {
            cols: 200,
            rows: 50,
            cell_w: 8,
            cell_h: 16,
        };
        let (_h, wire) = pipeline(size, 10_000, colors, None, None, Arc::new(|| {}));
        let line =
            b"2026-10-13 09:41:07 INFO \x1b[32mworker-17\x1b[0m processed request id=0123456789abcdef in 12ms\r\n";
        let chunk: Vec<u8> = line.iter().copied().cycle().take(64 * 1024).collect();
        let total = 100 * 1024 * 1024;
        let t = std::time::Instant::now();
        for _ in 0..total / chunk.len() {
            wire.feed(&chunk);
        }
        let secs = t.elapsed().as_secs_f64();
        println!("100 MB in {secs:.2} s = {:.0} MB/s", 100.0 / secs);
    }
}

#[cfg(test)]
mod mem {
    use super::*;

    fn rss_kb() -> u64 {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0))
            .unwrap_or(0)
    }

    /// `cargo test --release -p moliterm-core tab_memory -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn tab_memory() {
        let colors = Arc::new(RwLock::new(
            crate::theme::load_all(std::path::Path::new("/x"))[0].term.clone(),
        ));
        let size = Size {
            cols: 200,
            rows: 50,
            cell_w: 8,
            cell_h: 16,
        };
        let before = rss_kb();
        let (_h, wire) = pipeline(size, 10_000, colors, None, None, Arc::new(|| {}));
        let line: String = format!("{}\r\n", "x".repeat(199));
        for _ in 0..10_100 {
            wire.feed(line.as_bytes());
        }
        let after = rss_kb();
        println!(
            "cell = {} bytes; 200x50 + 10k scrollback: +{} MB",
            std::mem::size_of::<alacritty_terminal::term::cell::Cell>(),
            (after - before) / 1024
        );
    }
}
