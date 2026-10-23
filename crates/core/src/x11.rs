//! X11 forwarding: remote X clients reach the local X server (XQuartz,
//! VcXsrv, Xorg/XWayland). The server gets a random fake cookie; like
//! OpenSSH we only accept forwarded connections that present it, then strip
//! the auth before the local server, which applies its local-user rules.

use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, PartialEq)]
pub enum Display {
    Unix(PathBuf),
    Tcp(String, u16),
}

/// `""` → `$DISPLAY` → `:0`. Understands `:0`, `:0.1`, `unix:0`, `host:0`
/// and XQuartz's launchd socket path (`/private/tmp/…/org.xquartz:0`).
pub fn display(setting: &str) -> Display {
    let s = Some(setting.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()))
        .unwrap_or_else(|| ":0".into());
    parse(&s)
}

pub fn parse(s: &str) -> Display {
    if s.starts_with('/') {
        return Display::Unix(s.into());
    }
    let (host, rest) = s.rsplit_once(':').unwrap_or(("", s));
    let n: u16 = rest.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    match host {
        "" | "unix" if cfg!(unix) => Display::Unix(format!("/tmp/.X11-unix/X{n}").into()),
        "" | "unix" => Display::Tcp("127.0.0.1".into(), 6000 + n),
        h => Display::Tcp(h.into(), 6000 + n),
    }
}

/// 128-bit random hex for the `x11-req` (never a real local cookie).
pub fn fake_cookie() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// Reads the client's connection setup, checks the fake cookie and returns
/// the setup without auth data.
async fn setup_without_auth<R: AsyncRead + Unpin>(r: &mut R, cookie: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head).await?;
    let u16_at = |i: usize| {
        if head[0] == b'B' {
            u16::from_be_bytes([head[i], head[i + 1]])
        } else {
            u16::from_le_bytes([head[i], head[i + 1]])
        }
    };
    let (name_len, data_len) = (usize::from(u16_at(6)), usize::from(u16_at(8)));
    let mut auth = vec![0u8; pad4(name_len) + pad4(data_len)];
    r.read_exact(&mut auth).await?;
    let name = &auth[..name_len];
    let data = &auth[pad4(name_len)..pad4(name_len) + data_len];
    if name != b"MIT-MAGIC-COOKIE-1" || data != cookie {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "x11: wrong cookie",
        ));
    }
    head[6..10].fill(0);
    Ok(head.to_vec())
}

async fn forward<S, L>(mut remote: S, mut local: L, cookie: &[u8])
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: AsyncRead + AsyncWrite + Unpin,
{
    let setup = match setup_without_auth(&mut remote, cookie).await {
        Ok(s) => s,
        Err(e) => return tracing::warn!("{e}"),
    };
    if local.write_all(&setup).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut remote, &mut local).await;
}

/// Serves one forwarded X11 channel; `cookie` is the hex fake cookie.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send>(remote: S, display: Display, cookie: String) {
    let cookie: Vec<u8> = (0..cookie.len() / 2)
        .filter_map(|i| u8::from_str_radix(cookie.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    match display {
        #[cfg(unix)]
        Display::Unix(path) => {
            if let Ok(local) = tokio::net::UnixStream::connect(&path).await {
                forward(remote, local, &cookie).await;
            } else {
                tracing::warn!("x11: no X server at {}", path.display());
            }
        }
        #[cfg(not(unix))]
        Display::Unix(_) => {}
        Display::Tcp(host, port) => match tokio::net::TcpStream::connect((host.as_str(), port)).await {
            Ok(local) => forward(remote, local, &cookie).await,
            Err(e) => tracing::warn!("x11: no X server at {host}:{port}: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays() {
        assert_eq!(parse("localhost:10.0"), Display::Tcp("localhost".into(), 6010));
        assert_eq!(
            parse("/private/tmp/com.apple.launchd.x/org.xquartz:0"),
            Display::Unix("/private/tmp/com.apple.launchd.x/org.xquartz:0".into())
        );
        if cfg!(unix) {
            assert_eq!(parse(":1"), Display::Unix("/tmp/.X11-unix/X1".into()));
        } else {
            assert_eq!(parse(":1"), Display::Tcp("127.0.0.1".into(), 6001));
        }
        assert_eq!(fake_cookie().len(), 32);
    }

    #[tokio::test]
    async fn strips_auth() {
        // Little-endian setup: proto 11.0, auth name "MIT-MAGIC-COOKIE-1" (18 → 20), data 16 bytes.
        let mut setup = vec![b'l', 0, 11, 0, 0, 0, 18, 0, 16, 0, 0, 0];
        setup.extend(b"MIT-MAGIC-COOKIE-1\0\0");
        setup.extend([7u8; 16]);
        setup.extend(b"rest");
        let mut r = &setup[..];
        let out = setup_without_auth(&mut r, &[7u8; 16]).await.unwrap();
        assert_eq!(out, vec![b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r, b"rest");
        let mut r = &setup[..];
        assert!(setup_without_auth(&mut r, &[8u8; 16]).await.is_err());
    }
}
