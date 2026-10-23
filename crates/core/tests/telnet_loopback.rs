//! Telnet client against an in-process fake telnetd: negotiation, auto-login
//! with the password from the "vault", echo, close.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use moliterm_core::{
    ssh::{Ask, Ctx},
    telnet::{self, AutoLogin, Extra},
    terminal::{self, SessionEvent, Size},
    theme,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use zeroize::Zeroizing;

/// Reads until `needle` shows up; returns everything read.
async fn read_until(s: &mut tokio::net::TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut got = vec![];
    let mut buf = [0u8; 256];
    while !got.windows(needle.len()).any(|w| w == needle) {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "client hung up; got {got:?}");
        got.extend_from_slice(&buf[..n]);
    }
    got
}

#[test]
fn telnet_auto_login() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = rt.spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        // IAC DO NAWS, IAC DO TTYPE, IAC WILL ECHO, IAC SB TTYPE SEND IAC SE
        s.write_all(&[255, 253, 31, 255, 253, 24, 255, 251, 1, 255, 250, 24, 1, 255, 240])
            .await
            .unwrap();
        let neg = read_until(&mut s, b"xterm-256color\xff\xf0").await;
        assert!(neg.windows(3).any(|w| w == [255, 251, 31]), "WILL NAWS");
        assert!(neg.windows(3).any(|w| w == [255, 253, 1]), "DO ECHO");
        s.write_all(b"router login: ").await.unwrap();
        read_until(&mut s, b"admin\r\n").await;
        s.write_all(b"Password: ").await.unwrap();
        read_until(&mut s, b"s3cret\r\n").await;
        s.write_all(b"\r\nWelcome\r\n# ").await.unwrap();
        let line = read_until(&mut s, b"\r\n").await;
        s.write_all(&line).await.unwrap();
        let _ = s.read(&mut [0u8; 16]).await;
    });

    let colors = Arc::new(RwLock::new(theme::load_all(std::path::Path::new("/x"))[0].term.clone()));
    let size = Size {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 16,
    };
    let (handle, wire) = terminal::pipeline(size, 100, colors, None, None, Arc::new(|| {}));
    let (ask_tx, mut asks) = tokio::sync::mpsc::unbounded_channel();
    rt.spawn(async move {
        while let Some(a) = asks.recv().await {
            if let Ask::Secret { id, reply } = a {
                assert_eq!(id, "session:t:password");
                let _ = reply.send(Some(Zeroizing::new("s3cret".into())));
            }
        }
    });
    let ctx = Ctx {
        ask: ask_tx,
        agent_keys: Default::default(),
        wake: Arc::new(|| {}),
    };
    let ex = Extra::default();
    let opts = telnet::Options {
        host: "127.0.0.1".into(),
        port,
        raw: false,
        login: Some(AutoLogin {
            user: "admin".into(),
            secret_id: "session:t".into(),
            login_re: ex.login_prompt,
            password_re: ex.password_prompt,
        }),
    };
    rt.spawn(telnet::run(opts, ctx, wire, size));

    let deadline = Instant::now() + Duration::from_secs(5);
    let wait = |what: &str| loop {
        if terminal::all_text(&*handle.term.lock()).contains(what) {
            break;
        }
        assert!(Instant::now() < deadline, "no {what:?} on screen");
        std::thread::sleep(Duration::from_millis(20));
    };
    wait("Welcome");
    handle.send(&b"show ver\r"[..]);
    wait("# show ver");
    let evs: Vec<_> = handle.events.try_iter().collect();
    assert!(evs.contains(&SessionEvent::Connected));
    handle.send(&b"x"[..]);
    let _ = handle.cmd.send(terminal::Cmd::Close);
    rt.block_on(server).unwrap();
}
