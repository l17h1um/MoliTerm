//! Minimal SOCKS5 (no auth) and HTTP CONNECT client handshakes (SPEC §5.1.6).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn socks5<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, host: &str, port: u16) -> Result<(), String> {
    let e = |e: std::io::Error| e.to_string();
    s.write_all(&[5, 1, 0]).await.map_err(e)?;
    let mut hello = [0u8; 2];
    s.read_exact(&mut hello).await.map_err(e)?;
    if hello != [5, 0] {
        return Err("SOCKS5 proxy requires authentication".into());
    }
    let name = host.as_bytes();
    if name.len() > 255 {
        return Err("host name too long".into());
    }
    let mut req = vec![5, 1, 0, 3, name.len() as u8];
    req.extend_from_slice(name);
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await.map_err(e)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(e)?;
    if head[1] != 0 {
        return Err(format!("SOCKS5 proxy refused the connection (code {})", head[1]));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await.map_err(e)?;
            n[0] as usize
        }
        _ => return Err("bad SOCKS5 reply".into()),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await.map_err(e)?;
    Ok(())
}

pub async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, host: &str, port: u16) -> Result<(), String> {
    let e = |e: std::io::Error| e.to_string();
    s.write_all(format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n").as_bytes())
        .await
        .map_err(e)?;
    // Read byte by byte up to the blank line so no tunnel bytes are consumed.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.map_err(e)?;
        head.push(b[0]);
        if head.len() > 16 * 1024 {
            return Err("HTTP proxy reply too long".into());
        }
    }
    let status = String::from_utf8_lossy(&head);
    let line = status.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("HTTP proxy: {line}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socks5_and_http_handshakes() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 3];
            b.read_exact(&mut buf).await.unwrap();
            b.write_all(&[5, 0]).await.unwrap();
            let mut req = [0u8; 5 + 4 + 2];
            b.read_exact(&mut req).await.unwrap();
            assert_eq!(&req[5..9], b"host");
            b.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 22]).await.unwrap();
        });
        socks5(&mut a, "host", 22).await.unwrap();
        server.await.unwrap();

        let (mut a, mut b) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 64];
            let n = b.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("CONNECT h:22 HTTP/1.1"));
            b.write_all(b"HTTP/1.1 200 Connection established\r\n\r\nSSH-")
                .await
                .unwrap();
        });
        http_connect(&mut a, "h", 22).await.unwrap();
        let mut rest = [0u8; 4];
        a.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"SSH-");
        server.await.unwrap();
    }
}
