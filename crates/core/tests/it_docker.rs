//! Against `dev/docker-compose.yml`; run with `cargo test --features it`.
#![cfg(feature = "it")]

use std::sync::Arc;

use moliterm_core::ssh::{self, AgentKind, Ask, AuthMethod, Hop, HostKeyAnswer};
use zeroize::Zeroizing;

fn hop(host: &str, port: u16, auth: AuthMethod, key: Option<&str>) -> Hop {
    Hop {
        label: host.into(),
        host: host.into(),
        port,
        user: "test".into(),
        auth,
        key_path: key.map(|k| format!("{}/../../dev/keys/{k}", env!("CARGO_MANIFEST_DIR"))),
        agent: AgentKind::None,
        secret_id: "it".into(),
    }
}

fn ctx() -> ssh::Ctx {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ask) = rx.recv().await {
            match ask {
                Ask::HostKey { reply, .. } => {
                    let _ = reply.send(HostKeyAnswer::Once);
                }
                Ask::Secret { reply, .. } => {
                    let _ = reply.send(None);
                }
                Ask::Password { kind, reply, .. } => {
                    let v = match kind {
                        ssh::PromptKind::Passphrase(_) => "testpass",
                        ssh::PromptKind::Password(_) => "test",
                    };
                    let _ = reply.send(Some((Zeroizing::new(v.into()), false)));
                }
                Ask::KeyboardInteractive { prompts, reply, .. } => {
                    let _ = reply.send(Some(prompts.iter().map(|_| Zeroizing::new("test".into())).collect()));
                }
                _ => {}
            }
        }
    });
    ssh::Ctx {
        ask: tx,
        agent_keys: Default::default(),
        wake: Arc::new(|| {}),
    }
}

fn opts(target: Hop, jumps: Vec<Hop>) -> ssh::Options {
    ssh::Options {
        target,
        jumps,
        proxy: None,
        keepalive: true,
        reconnect: false,
        legacy: false,
        after_login: None,
        agent_forward: false,
        known_hosts: std::env::temp_dir().join("moliterm-it-known_hosts"),
        notices: Default::default(),
        lang: Some("C.UTF-8".into()),
        x11: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn password_key_encrypted_key_and_bastion() {
    let c = ctx();
    ssh::connect(&opts(hop("127.0.0.1", 2222, AuthMethod::Password, None), vec![]), &c)
        .await
        .unwrap();
    ssh::connect(
        &opts(hop("127.0.0.1", 2222, AuthMethod::Key, Some("test_ed25519")), vec![]),
        &c,
    )
    .await
    .unwrap();
    ssh::connect(
        &opts(
            hop("127.0.0.1", 2222, AuthMethod::Key, Some("test_ed25519_enc")),
            vec![],
        ),
        &c,
    )
    .await
    .unwrap();
    let jump = hop("127.0.0.1", 2224, AuthMethod::Password, None);
    ssh::connect(&opts(hop("target", 22, AuthMethod::Password, None), vec![jump]), &c)
        .await
        .unwrap();
}

/// xrdp from docker-compose (`localhost:3390`, test / test): log in, get a
/// picture, then ask for a new size.
#[tokio::test]
async fn rdp_xrdp() {
    use moliterm_core::{rdp, terminal::SessionEvent};
    let dir = std::env::temp_dir().join(format!("moliterm-it-rdp-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let (handle, link) = rdp::pipeline(Arc::new(|| {}));
    let opts = rdp::Options {
        host: "127.0.0.1".into(),
        port: 3390,
        user: "test".into(),
        domain: String::new(),
        secret_id: "it".into(),
        known_hosts: dir.join("rdp_known_hosts"),
        size: (1024, 768),
    };
    tokio::spawn(rdp::run(opts, ctx(), link));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let wait = |what: &str| {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
    };
    // Connected, then a frame with something on it.
    loop {
        match handle.events.try_recv() {
            Ok(SessionEvent::Connected) => break,
            Ok(SessionEvent::Disconnected(e)) => panic!("{e}"),
            _ => wait("connect"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    loop {
        let painted = handle
            .frame
            .lock()
            .map(|f| f.width == 1024 && f.rgba.chunks(4).any(|p| p[..3] != [0, 0, 0]))
            .unwrap_or(false);
        if painted {
            break;
        }
        wait("pixels");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    handle.send(rdp::Input::Resize {
        width: 800,
        height: 600,
    });
    loop {
        if handle.frame.lock().map(|f| (f.width, f.height)).unwrap_or_default() == (800, 600) {
            break;
        }
        if let Ok(SessionEvent::Disconnected(e)) = handle.events.try_recv() {
            panic!("{e}");
        }
        wait("resize");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    handle.send(rdp::Input::Close);
}

/// -L, -D and -R through the docker sshd; each reads the server's own SSH
/// banner (-L/-D) or a local "hello" service from inside the container (-R).
#[tokio::test(flavor = "multi_thread")]
async fn tunnels_local_dynamic_remote() {
    use moliterm_core::tunnel::{self, Kind, State, Stats, Tunnel};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let o = opts(hop("127.0.0.1", 2222, AuthMethod::Password, None), vec![]);
    let start = |t: Tunnel| {
        let stats = Arc::new(Stats::default());
        let (stop, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(tunnel::run(t, o.clone(), ctx(), stats.clone(), rx));
        (stats, stop)
    };
    let wait_up = |s: &Arc<Stats>| {
        let s = s.clone();
        async move {
            for _ in 0..100 {
                match s.state() {
                    State::Up => return,
                    State::Error(e) => panic!("{e}"),
                    _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            }
            panic!("tunnel did not come up");
        }
    };
    let banner = |mut s: tokio::net::TcpStream| async move {
        let mut b = [0u8; 7];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"SSH-2.0");
    };

    let (l, _stop_l) = start(Tunnel {
        kind: Kind::Local,
        listen_port: 22201,
        target_host: "127.0.0.1".into(),
        target_port: 22,
        ..Default::default()
    });
    wait_up(&l).await;
    banner(tokio::net::TcpStream::connect("127.0.0.1:22201").await.unwrap()).await;
    assert!(l.rx.load(std::sync::atomic::Ordering::Relaxed) >= 7);

    let (d, _stop_d) = start(Tunnel {
        kind: Kind::Dynamic,
        listen_port: 22202,
        ..Default::default()
    });
    wait_up(&d).await;
    let mut s = tokio::net::TcpStream::connect("127.0.0.1:22202").await.unwrap();
    moliterm_core::proxy::socks5(&mut s, "localhost", 22).await.unwrap();
    banner(s).await;

    let svc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let svc_port = svc.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = svc.accept().await {
            let _ = c.write_all(b"hello").await;
        }
    });
    let (r, stop_r) = start(Tunnel {
        kind: Kind::Remote,
        bind: "127.0.0.1".into(),
        listen_port: 22203,
        target_host: "127.0.0.1".into(),
        target_port: svc_port,
        ..Default::default()
    });
    wait_up(&r).await;
    let out = std::process::Command::new("docker")
        .args(["exec", "dev-sshd-1", "sh", "-c", "nc -w 2 127.0.0.1 22203 </dev/null"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    stop_r.send(true).unwrap();
    for _ in 0..50 {
        if r.state() == State::Stopped {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("tunnel did not stop");
}

/// Monitor against the docker Linux host.
#[tokio::test(flavor = "multi_thread")]
async fn monitor_linux() {
    use moliterm_core::monitor;
    let c = ctx();
    let (h, _) = ssh::connect(&opts(hop("127.0.0.1", 2222, AuthMethod::Password, None), vec![]), &c)
        .await
        .unwrap();
    let live: ssh::Live = Arc::new(std::sync::Mutex::new(Some(h)));
    let shared = Arc::new(monitor::Shared::default());
    tokio::spawn(monitor::run(live, shared.clone()));
    for _ in 0..80 {
        shared.touch();
        if let Some(m) = shared.metrics.lock().unwrap().clone() {
            assert!(m.mem_total > 0 && m.uptime > 0, "{m:?}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("no metrics");
}

/// busybox telnetd (`localhost:2323`): auto-login with the prompted password, run a command.
#[tokio::test(flavor = "multi_thread")]
async fn telnet_busybox_auto_login() {
    use moliterm_core::{telnet, terminal};
    let colors = Arc::new(std::sync::RwLock::new(
        moliterm_core::theme::load_all(std::path::Path::new("/x"))[0]
            .term
            .clone(),
    ));
    let size = terminal::Size {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 16,
    };
    let (handle, wire) = terminal::pipeline(size, 100, colors, None, None, Arc::new(|| {}));
    let ex = telnet::Extra::default();
    let o = telnet::Options {
        host: "127.0.0.1".into(),
        port: 2323,
        raw: false,
        login: Some(telnet::AutoLogin {
            user: "test".into(),
            secret_id: "it".into(),
            login_re: ex.login_prompt,
            password_re: ex.password_prompt,
        }),
    };
    tokio::spawn(telnet::run(o, ctx(), wire, size));
    let text = || terminal::all_text(&*handle.term.lock());
    let mut sent = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let t = text();
        if !sent && (t.trim_end().ends_with('$') || t.trim_end().ends_with('#')) {
            handle.send(&b"echo moli$((40+2))\r"[..]);
            sent = true;
        }
        if t.contains("moli42") {
            return;
        }
    }
    panic!("no shell output:\n{}", text());
}

/// X11 forwarding: a remote client connecting to $DISPLAY reaches a fake local
/// X server, which must see the setup packet with the auth stripped.
#[tokio::test(flavor = "multi_thread")]
async fn x11_forwarding() {
    use moliterm_core::terminal;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let xsrv = tokio::net::TcpListener::bind("127.0.0.1:6099").await.unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = xsrv.accept().await {
            let mut head = [0u8; 12];
            if c.read_exact(&mut head).await.is_ok() {
                let reply: &[u8] = if head[6..10] == [0, 0, 0, 0] {
                    b"X-OK"
                } else {
                    b"X-AUTH"
                };
                let _ = c.write_all(reply).await;
            }
        }
    });
    let cookie = moliterm_core::x11::fake_cookie();
    let mut packet = vec![b'l', 0, 11, 0, 0, 0, 18, 0, 16, 0, 0, 0];
    packet.extend(b"MIT-MAGIC-COOKIE-1\0\0");
    packet.extend((0..16).map(|i| u8::from_str_radix(&cookie[i * 2..i * 2 + 2], 16).unwrap()));
    let octal: String = packet.iter().map(|b| format!("\\{b:03o}")).collect();
    let mut o = opts(hop("127.0.0.1", 2222, AuthMethod::Password, None), vec![]);
    o.x11 = Some(("127.0.0.1:99".into(), cookie));
    let colors = Arc::new(std::sync::RwLock::new(
        moliterm_core::theme::load_all(std::path::Path::new("/x"))[0]
            .term
            .clone(),
    ));
    let size = terminal::Size {
        cols: 120,
        rows: 30,
        cell_w: 8,
        cell_h: 16,
    };
    let (handle, wire) = terminal::pipeline(size, 100, colors, None, None, Arc::new(|| {}));
    tokio::spawn(ssh::run(o, ctx(), wire, size, Default::default()));
    let text = || terminal::all_text(&*handle.term.lock());
    for _ in 0..50 {
        if text().contains('$') {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let cmd = format!(
        "echo D=$DISPLAY; n=${{DISPLAY#*:}}; n=${{n%.*}}; printf '{octal}' | nc -w 2 127.0.0.1 $((6000+n)); echo; echo done\r"
    );
    handle.send(cmd.into_bytes());
    for _ in 0..60 {
        let t = text();
        if t.contains("X-OK") {
            return;
        }
        assert!(!t.contains("X-AUTH"), "auth not stripped");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("no reply through X11 forwarding:\n{}", text());
}

/// An identity's private key lives in the vault (`key_path = vault:identity:<id>:key`).
#[tokio::test(flavor = "multi_thread")]
async fn identity_key_from_vault() {
    let key_text =
        std::fs::read_to_string(format!("{}/../../dev/keys/test_ed25519", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ask) = rx.recv().await {
            match ask {
                Ask::HostKey { reply, .. } => drop(reply.send(HostKeyAnswer::Once)),
                Ask::Secret { id, reply } => {
                    let v = (id == "identity:abc:key").then(|| Zeroizing::new(key_text.clone()));
                    drop(reply.send(v));
                }
                Ask::Password { reply, .. } => drop(reply.send(None)),
                _ => {}
            }
        }
    });
    let c = ssh::Ctx {
        ask: tx,
        agent_keys: Default::default(),
        wake: Arc::new(|| {}),
    };
    let mut target = hop("127.0.0.1", 2222, AuthMethod::Key, None);
    target.key_path = Some(moliterm_core::identity::key_ref("abc"));
    target.secret_id = moliterm_core::identity::prefix("abc");
    ssh::connect(&opts(target, vec![]), &c).await.unwrap();
}
