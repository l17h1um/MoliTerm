//! Remote file access against an in-process SSH + SFTP server with an
//! in-memory file system: list, upload, download, resume by offset, chmod,
//! rename, mkdir, delete, and the transfer queue.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use moliterm_core::{
    remote::Remote,
    ssh::{self, AgentKind, Ask, AuthMethod, Hop, HostKeyAnswer},
    transfer::{Dir, OnConflict, Queue, State},
};
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, ssh_key::rand_core::UnwrapErr},
    server::{self, Auth, Msg, Session},
};
use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

/// path → (content, permissions, is_dir)
type Fs = Arc<Mutex<HashMap<String, (Vec<u8>, u32, bool)>>>;

#[derive(Clone)]
struct Ssh {
    fs: Fs,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "pw" { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        let channel = self.channels.lock().unwrap().remove(&id);
        match (name, channel) {
            ("sftp", Some(ch)) => {
                session.channel_success(id)?;
                let sftp = Sftp {
                    fs: self.fs.clone(),
                    dirs: HashMap::new(),
                };
                tokio::spawn(russh_sftp::server::run(ch.into_stream(), sftp));
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

struct Sftp {
    fs: Fs,
    /// Directory handles already listed.
    dirs: HashMap<String, bool>,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en".into(),
    }
}

fn attrs(content: &[u8], perms: u32, is_dir: bool) -> FileAttributes {
    let mut a = FileAttributes::empty();
    a.size = Some(content.len() as u64);
    a.permissions = Some(perms | if is_dir { 0o040000 } else { 0o100000 });
    a.mtime = Some(1_700_000_000);
    a
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let p = if path == "." { "/home".to_owned() } else { path };
        Ok(Name {
            id,
            files: vec![File::dummy(p)],
        })
    }

    async fn open(
        &mut self,
        id: u32,
        path: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        if flags.contains(OpenFlags::CREATE) && !fs.contains_key(&path) {
            fs.insert(path.clone(), (vec![], 0o644, false));
        }
        if flags.contains(OpenFlags::TRUNCATE)
            && let Some(f) = fs.get_mut(&path)
        {
            f.0.clear();
        }
        if !fs.contains_key(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(Handle { id, handle: path })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.dirs.remove(&handle);
        Ok(ok(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let fs = self.fs.lock().unwrap();
        let (content, ..) = fs.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let start = offset as usize;
        if start >= content.len() {
            return Err(StatusCode::Eof);
        }
        let end = (start + len as usize).min(content.len());
        Ok(Data {
            id,
            data: content[start..end].to_vec(),
        })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        let (content, ..) = fs.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(&data);
        Ok(ok(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, path).await
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let fs = self.fs.lock().unwrap();
        let (c, p, d) = fs.get(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: attrs(c, *p, *d),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, handle).await
    }

    async fn setstat(&mut self, id: u32, path: String, a: FileAttributes) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        let f = fs.get_mut(&path).ok_or(StatusCode::NoSuchFile)?;
        if let Some(p) = a.permissions {
            f.1 = p & 0o7777;
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.dirs.insert(path.clone(), false);
        Ok(Handle { id, handle: path })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        if self.dirs.get(&handle).copied().unwrap_or(true) {
            return Err(StatusCode::Eof);
        }
        self.dirs.insert(handle.clone(), true);
        let prefix = if handle.ends_with('/') {
            handle.clone()
        } else {
            format!("{handle}/")
        };
        let fs = self.fs.lock().unwrap();
        let files = fs
            .iter()
            .filter_map(|(p, (c, perm, d))| {
                let name = p.strip_prefix(&prefix)?;
                (!name.is_empty() && !name.contains('/')).then(|| File::new(name, attrs(c, *perm, *d)))
            })
            .collect();
        Ok(Name { id, files })
    }

    async fn mkdir(&mut self, id: u32, path: String, _: FileAttributes) -> Result<Status, Self::Error> {
        self.fs.lock().unwrap().insert(path, (vec![], 0o755, true));
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.fs.lock().unwrap().remove(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.remove(id, path).await
    }

    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        let f = fs.remove(&from).ok_or(StatusCode::NoSuchFile)?;
        fs.insert(to, f);
        Ok(ok(id))
    }
}

async fn start_server(fs: Fs) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), Algorithm::Ed25519).unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let h = Ssh {
                fs: fs.clone(),
                channels: Default::default(),
            };
            let cfg = config.clone();
            tokio::spawn(async move {
                if let Ok(r) = server::run_stream(cfg, stream, h).await {
                    let _ = r.await;
                }
            });
        }
    });
    port
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
                Ask::Password { reply, .. } => {
                    let _ = reply.send(Some((Zeroizing::new("pw".into()), false)));
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
async fn sftp_ops_and_resumable_queue() {
    let fs: Fs = Default::default();
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    {
        let mut f = fs.lock().unwrap();
        f.insert("/home".into(), (vec![], 0o755, true));
        f.insert("/home/big.bin".into(), (big.clone(), 0o644, false));
        f.insert("/home/notes.txt".into(), (b"hello".to_vec(), 0o600, false));
    }
    let port = start_server(fs.clone()).await;
    let dir = std::env::temp_dir().join(format!("moliterm-sftp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let opts = ssh::Options {
        target: Hop {
            label: "t".into(),
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            auth: AuthMethod::Password,
            key_path: None,
            agent: AgentKind::None,
            secret_id: "t".into(),
        },
        jumps: vec![],
        proxy: None,
        keepalive: false,
        reconnect: false,
        legacy: false,
        after_login: None,
        agent_forward: false,
        known_hosts: dir.join("known_hosts"),
        notices: Default::default(),
        lang: Some("C.UTF-8".into()),
        x11: None,
    };
    let (handle, _) = ssh::connect(&opts, &ctx()).await.unwrap();
    let remote = Arc::new(Remote::open(&handle).await.unwrap());
    assert!(!remote.is_scp());
    assert_eq!(remote.home().await.unwrap(), "/home");

    let mut names: Vec<_> = remote
        .list("/home")
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.size, e.perms))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [("big.bin".into(), 700_000, 0o644), ("notes.txt".into(), 5, 0o600)]
    );

    remote.chmod("/home/notes.txt", 0o640).await.unwrap();
    remote.rename("/home/notes.txt", "/home/n.txt").await.unwrap();
    remote.mkdir("/home/sub").await.unwrap();
    assert!(
        remote
            .list("/home")
            .await
            .unwrap()
            .iter()
            .any(|e| e.name == "sub" && e.is_dir)
    );
    assert_eq!(fs.lock().unwrap()["/home/n.txt"].1, 0o640);

    // Download resume: a partial local file is continued, not restarted.
    let local = dir.join("big.bin");
    std::fs::write(&local, &big[..123_457]).unwrap();
    let rt = tokio::runtime::Handle::current();
    let wake: moliterm_core::terminal::Waker = Arc::new(|| {});
    let mut q = Queue::default();
    q.push(
        Dir::Down,
        local.clone(),
        "/home/big.bin".into(),
        big.len() as u64,
        OnConflict::Ask,
    );
    // Upload resume: a partial remote file is continued.
    let up_local = dir.join("up.bin");
    std::fs::write(&up_local, &big).unwrap();
    fs.lock()
        .unwrap()
        .insert("/home/up.bin".into(), (big[..50_000].to_vec(), 0o644, false));
    q.push(
        Dir::Up,
        up_local,
        "/home/up.bin".into(),
        big.len() as u64,
        OnConflict::Resume,
    );
    q.pump(&remote, 3, &rt, &wake);
    let wait = async |q: &Queue, id: usize, want: fn(&State) -> bool| {
        for _ in 0..200 {
            if want(&q.items[id].state()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("transfer {id} stuck in {:?}", q.items[id].state());
    };
    wait(&q, 0, |s| matches!(s, State::Conflict(123_457))).await;
    q.decide(1, OnConflict::Resume, false);
    q.pump(&remote, 3, &rt, &wake);
    wait(&q, 0, |s| *s == State::Done).await;
    wait(&q, 1, |s| *s == State::Done).await;
    assert_eq!(std::fs::read(&local).unwrap(), big);
    assert_eq!(fs.lock().unwrap()["/home/up.bin"].0, big);

    remote.remove("/home/sub", true).await.unwrap();
    remote.remove("/home/n.txt", false).await.unwrap();
    assert_eq!(remote.list("/home").await.unwrap().len(), 2);
    std::fs::remove_dir_all(dir).unwrap();
}
