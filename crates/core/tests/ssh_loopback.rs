//! SSH client against an in-process russh server on localhost: host key
//! prompt + known_hosts, password auth, shell I/O, changed-key rejection.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use moliterm_core::{
    known_hosts,
    ssh::{self, AgentKind, Ask, AuthMethod, Hop, HostKeyAnswer},
    terminal::{self, Size},
};
use russh::{
    Channel, ChannelId, MethodKind, MethodSet,
    keys::{Algorithm, PrivateKey, ssh_key::rand_core::UnwrapErr},
    server::{self, Auth, Msg, Session},
};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

#[derive(Clone)]
struct Srv(Arc<std::sync::Mutex<Vec<String>>>);

impl server::Handler for Srv {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "u" && password == "secret" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(format!("{name}={value}"));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, &b"welcome\r\n"[..])?;
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        Ok(())
    }
}

async fn server(key: PrivateKey, env: Arc<std::sync::Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(server::Config {
        keys: vec![key],
        methods: MethodSet::from(&[MethodKind::Password][..]),
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let cfg = config.clone();
            let env = env.clone();
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(cfg, stream, Srv(env.clone())).await {
                    let _ = running.await;
                }
            });
        }
    });
    port
}

fn key() -> PrivateKey {
    PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), Algorithm::Ed25519).unwrap()
}

fn options(port: u16, known: &std::path::Path) -> ssh::Options {
    ssh::Options {
        target: Hop {
            label: "test".into(),
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            auth: AuthMethod::Password,
            key_path: None,
            agent: AgentKind::None,
            secret_id: "session:test".into(),
        },
        jumps: vec![],
        proxy: None,
        keepalive: true,
        reconnect: false,
        legacy: false,
        after_login: None,
        agent_forward: false,
        known_hosts: known.to_path_buf(),
        notices: Default::default(),
        lang: Some("C.UTF-8".into()),
        x11: None,
    }
}

/// Answers every question like a user would: trust new keys, reject changed
/// ones, type "secret" as the password. Counts host key questions.
fn ctx(host_key_asks: Arc<AtomicUsize>) -> ssh::Ctx {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ask) = rx.recv().await {
            match ask {
                Ask::HostKey { changed, reply, .. } => {
                    host_key_asks.fetch_add(1, Ordering::SeqCst);
                    let _ = reply.send(if changed.is_some() {
                        HostKeyAnswer::Reject
                    } else {
                        HostKeyAnswer::Save
                    });
                }
                Ask::Secret { reply, .. } => {
                    let _ = reply.send(None);
                }
                Ask::Password { reply, .. } => {
                    let _ = reply.send(Some((Zeroizing::new("secret".into()), false)));
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

#[tokio::test(flavor = "multi_thread")]
async fn hostkey_password_shell_and_changed_key() {
    let dir = std::env::temp_dir().join(format!("moliterm-ssh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let known = dir.join("known_hosts");
    let first_key = key();
    let first_pub = first_key.public_key().clone();
    let env: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let port = server(first_key, env.clone()).await;
    let asks = Arc::new(AtomicUsize::new(0));
    let ctx = ctx(asks.clone());
    let opts = options(port, &known);

    // New host: one question, then the key is saved.
    let (h, key_type) = ssh::connect(&opts, &ctx).await.unwrap();
    assert_eq!(key_type, "ssh-ed25519");
    assert_eq!(asks.load(Ordering::SeqCst), 1);
    assert_eq!(
        known_hosts::check(&known, "127.0.0.1", port, &first_pub),
        known_hosts::Check::Known
    );
    drop(h);
    // Known host: no question.
    ssh::connect(&opts, &ctx).await.unwrap();
    assert_eq!(asks.load(Ordering::SeqCst), 1);

    // Shell: banner and echo reach the terminal.
    let colors = Arc::new(RwLock::new(moliterm_core::theme::load_all(&dir)[0].term.clone()));
    let size = Size {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 16,
    };
    let (handle, wire) = terminal::pipeline(size, 100, colors, None, None, Arc::new(|| {}));
    let live = Arc::new(std::sync::Mutex::new(None));
    let task = tokio::spawn(ssh::run(opts.clone(), ctx.clone(), wire, size, live.clone()));
    handle.send(&b"ping-42"[..]);
    let mut ok = false;
    for _ in 0..100 {
        let text = terminal::all_text(&handle.term.lock());
        if text.contains("welcome") && text.contains("ping-42") {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "shell output did not arrive");
    assert!(
        env.lock().unwrap().contains(&"LANG=C.UTF-8".to_owned()),
        "LANG not sent"
    );
    assert!(live.lock().unwrap().is_some());
    let _ = handle.cmd.send(terminal::Cmd::Close);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();

    // Changed key: the saved key for port2 differs from what the server presents.
    let port2 = server(key(), Default::default()).await;
    known_hosts::append(&known, "127.0.0.1", port2, &first_pub).unwrap();
    let err = ssh::connect(&options(port2, &known), &ctx).await.err().unwrap();
    assert!(err.fatal, "{}", err.msg);
    assert_eq!(asks.load(Ordering::SeqCst), 2);
    std::fs::remove_dir_all(dir).unwrap();
}
