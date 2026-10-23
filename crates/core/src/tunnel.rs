//! SSH tunnels (SPEC §5.6): local (-L), remote (-R) and dynamic SOCKS5 (-D).
//! Each running tunnel has its own SSH connection made from a saved
//! session's settings; no terminal tab is needed.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
};

use crate::{
    Error,
    db::{Db, now_ms},
    ssh::{self, Ctx},
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Local,
    Remote,
    Dynamic,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Tunnel {
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub sort: i64,
    pub kind: Kind,
    /// Saved SSH session whose settings the tunnel connects with.
    pub session_id: String,
    /// Where the listening side binds: locally for -L/-D, on the server for -R.
    pub bind: String,
    pub listen_port: u16,
    /// -L: reached from the server; -R: reached from this computer. Unused for -D.
    pub target_host: String,
    pub target_port: u16,
    /// Start when the app starts.
    pub autostart: bool,
    pub reconnect: bool,
    /// Start when its session is opened in a tab.
    pub with_session: bool,
}

impl Default for Tunnel {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            sort: 0,
            kind: Kind::Local,
            session_id: String::new(),
            bind: "127.0.0.1".into(),
            listen_port: 0,
            target_host: "127.0.0.1".into(),
            target_port: 0,
            autostart: false,
            reconnect: true,
            with_session: false,
        }
    }
}

impl Tunnel {
    /// `127.0.0.1:8080 → db:5432`, `SOCKS 127.0.0.1:1080`.
    pub fn route(&self) -> String {
        let listen = format!("{}:{}", self.bind, self.listen_port);
        let target = format!("{}:{}", self.target_host, self.target_port);
        match self.kind {
            Kind::Local => format!("{listen} → {target}"),
            Kind::Remote => format!("{listen} ← {target}"),
            Kind::Dynamic => format!("SOCKS {listen}"),
        }
    }

    /// Binding to anything but loopback exposes the port to the network.
    pub fn exposed(&self) -> bool {
        !matches!(self.bind.trim(), "127.0.0.1" | "localhost" | "::1" | "")
    }
}

pub fn load(db: &Db) -> Result<Vec<Tunnel>, Error> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name, sort, data FROM tunnels WHERE deleted_at IS NULL ORDER BY sort, name")?;
    let rows = stmt
        .query_map([], |r| {
            let data: String = r.get(3)?;
            let mut t: Tunnel = serde_json::from_str(&data).unwrap_or_default();
            t.id = r.get(0)?;
            t.name = r.get(1)?;
            t.sort = r.get(2)?;
            Ok(t)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn save(db: &Db, t: &Tunnel) -> Result<(), Error> {
    db.conn().execute(
        "INSERT INTO tunnels (id, name, sort, data, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = ?2, sort = ?3, data = ?4, updated_at = ?5, device_id = ?6",
        params![
            t.id,
            t.name,
            t.sort,
            serde_json::to_string(t)?,
            now_ms(),
            db.device_id()
        ],
    )?;
    Ok(())
}

pub fn delete(db: &Db, id: &str) -> Result<(), Error> {
    db.conn().execute(
        "UPDATE tunnels SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE id = ?1",
        params![id, now_ms(), db.device_id()],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum State {
    #[default]
    Stopped,
    Starting,
    Up,
    Error(String),
}

/// Live state and counters, shared with the UI.
#[derive(Default)]
pub struct Stats {
    pub state: Mutex<State>,
    /// Bytes from the remote side / to the remote side.
    pub rx: AtomicU64,
    pub tx: AtomicU64,
    /// Open forwarded connections.
    pub conns: AtomicU32,
}

impl Stats {
    fn set(&self, s: State) {
        if let Ok(mut st) = self.state.lock() {
            *st = s;
        }
    }

    pub fn state(&self) -> State {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Runs until `stop` turns true (or the tunnel fails for good).
pub async fn run(t: Tunnel, opts: ssh::Options, ctx: Ctx, stats: Arc<Stats>, mut stop: watch::Receiver<bool>) {
    stats.set(State::Starting);
    // Local listeners are bound once, before connecting, so a busy port fails fast.
    let listener = match t.kind {
        Kind::Remote => None,
        _ => {
            // A restarted tunnel may race its previous run for the port.
            let mut r = TcpListener::bind((t.bind.trim(), t.listen_port)).await;
            for _ in 0..10 {
                if r.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                r = TcpListener::bind((t.bind.trim(), t.listen_port)).await;
            }
            match r {
                Ok(l) => Some(l),
                Err(e) => return stats.set(State::Error(format!("{}:{}: {e}", t.bind, t.listen_port))),
            }
        }
    };
    let mut attempt = 0u32;
    loop {
        stats.set(State::Starting);
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();
        let forwarded = (t.kind == Kind::Remote).then_some(fwd_tx);
        let result = tokio::select! {
            r = ssh::connect_with(&opts, &ctx, forwarded) => r,
            _ = stop.changed() => return stats.set(State::Stopped),
        };
        let reason = match result {
            Err(f) if f.fatal || !t.reconnect => return stats.set(State::Error(f.msg)),
            Err(f) => f.msg,
            Ok((handle, _)) => {
                attempt = 0;
                let up = async {
                    if t.kind == Kind::Remote {
                        let bind = t.bind.trim().to_owned();
                        handle
                            .tcpip_forward(bind.clone(), t.listen_port.into())
                            .await
                            .map_err(|e| format!("server refused {bind}:{}: {e}", t.listen_port))?;
                    }
                    stats.set(State::Up);
                    let mut alive = tokio::time::interval(Duration::from_secs(2));
                    loop {
                        tokio::select! {
                            Ok((sock, peer)) = async { match &listener { Some(l) => l.accept().await, None => std::future::pending().await } } => {
                                let (h, s, kind) = (handle.clone(), stats.clone(), t.kind);
                                let target = (t.target_host.clone(), t.target_port);
                                tokio::spawn(async move { local_conn(h, sock, peer, kind, target, s).await });
                            }
                            Some(ch) = fwd_rx.recv() => {
                                let (s, target) = (stats.clone(), (t.target_host.clone(), t.target_port));
                                tokio::spawn(async move {
                                    if let Ok(sock) = TcpStream::connect((target.0.as_str(), target.1)).await {
                                        pump(sock, ch.into_stream(), &s).await;
                                    }
                                });
                            }
                            _ = alive.tick() => {
                                if handle.is_closed() {
                                    return Err::<(), String>("connection lost".into());
                                }
                            }
                        }
                    }
                };
                let r = tokio::select! {
                    r = up => r,
                    _ = stop.changed() => {
                        let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
                        return stats.set(State::Stopped);
                    }
                };
                match r {
                    Err(e) if !t.reconnect => return stats.set(State::Error(e)),
                    Err(e) => e,
                    Ok(()) => return stats.set(State::Stopped),
                }
            }
        };
        if attempt >= 10 {
            return stats.set(State::Error(reason));
        }
        stats.set(State::Error(reason));
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
        attempt += 1;
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.changed() => return stats.set(State::Stopped),
        }
    }
}

/// One accepted local connection of a -L or -D tunnel.
async fn local_conn(
    h: ssh::SshHandle,
    mut sock: TcpStream,
    peer: std::net::SocketAddr,
    kind: Kind,
    target: (String, u16),
    stats: Arc<Stats>,
) {
    let (host, port) = if kind == Kind::Dynamic {
        match socks5_accept(&mut sock).await {
            Ok(t) => t,
            Err(_) => return,
        }
    } else {
        target
    };
    let ch = h
        .channel_open_direct_tcpip(host, port.into(), peer.ip().to_string(), peer.port().into())
        .await;
    match ch {
        Ok(ch) => {
            if kind == Kind::Dynamic && sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.is_err() {
                return;
            }
            pump(sock, ch.into_stream(), &stats).await;
        }
        Err(_) if kind == Kind::Dynamic => {
            // "Connection refused"
            let _ = sock.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
        }
        Err(_) => {}
    }
}

/// SOCKS5 server side, no auth, CONNECT only (RFC 1928). Returns the target.
async fn socks5_accept<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> std::io::Result<(String, u16)> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(bad("not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1].into()];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).await?;
        return Err(bad("no acceptable auth method"));
    }
    s.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[1] != 1 {
        // Command not supported
        s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(bad("only CONNECT is supported"));
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0].into()];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| bad("bad host name"))?
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => return Err(bad("bad address type")),
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

/// Copies both ways until either side closes; counts bytes.
/// `local` → `remote` is `tx`, `remote` → `local` is `rx`.
async fn pump<L, R>(local: L, remote: R, stats: &Stats)
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    stats.conns.fetch_add(1, Ordering::Relaxed);
    let (mut lr, mut lw) = tokio::io::split(local);
    let (mut rr, mut rw) = tokio::io::split(remote);
    tokio::join!(
        async {
            copy(&mut lr, &mut rw, &stats.tx).await;
            let _ = rw.shutdown().await;
        },
        async {
            copy(&mut rr, &mut lw, &stats.rx).await;
            let _ = lw.shutdown().await;
        }
    );
    stats.conns.fetch_sub(1, Ordering::Relaxed);
}

async fn copy<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W, counter: &AtomicU64) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if w.write_all(&buf[..n]).await.is_err() {
            return;
        }
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socks5_handshake() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let srv = tokio::spawn(async move { socks5_accept(&mut server).await.map(|t| (t, server)) });
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        // CONNECT example.org:443 by name
        let mut req = vec![5, 1, 0, 3, 11];
        req.extend(b"example.org");
        req.extend(443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        let ((host, port), _) = srv.await.unwrap().unwrap();
        assert_eq!((host.as_str(), port), ("example.org", 443));
    }

    #[tokio::test]
    async fn pump_counts() {
        let stats = Stats::default();
        let (mut a_far, a) = tokio::io::duplex(64);
        let (b, mut b_far) = tokio::io::duplex(64);
        let task = async {
            a_far.write_all(b"hello").await.unwrap();
            let mut got = [0u8; 5];
            b_far.read_exact(&mut got).await.unwrap();
            b_far.write_all(b"hi").await.unwrap();
            let mut back = [0u8; 2];
            a_far.read_exact(&mut back).await.unwrap();
            drop(a_far);
            drop(b_far);
            (got, back)
        };
        let ((got, back), ()) = tokio::join!(task, pump(a, b, &stats));
        assert_eq!((&got, &back), (b"hello", b"hi"));
        assert_eq!(
            (stats.tx.load(Ordering::Relaxed), stats.rx.load(Ordering::Relaxed)),
            (5, 2)
        );
    }

    #[test]
    fn db_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let t = Tunnel {
            id: crate::sessions::new_id(),
            name: "pg".into(),
            kind: Kind::Dynamic,
            listen_port: 1080,
            ..Default::default()
        };
        save(&db, &t).unwrap();
        assert_eq!(load(&db).unwrap(), vec![t.clone()]);
        assert!(!t.exposed());
        delete(&db, &t.id).unwrap();
        assert!(load(&db).unwrap().is_empty());
    }
}
