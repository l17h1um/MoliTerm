//! Tiny SSH + SFTP server for UI checks without Docker:
//! `cargo run -p moliterm-core --example dev_sshd -- <root dir> [port]`.
//! Any user, password `test`. The shell echoes input; SFTP serves `<root dir>`.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, ssh_key::rand_core::UnwrapErr},
    server::{self, Auth, Msg, Session},
};
use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Clone)]
struct Ssh {
    root: PathBuf,
    channels: Arc<std::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// Channels running the echo shell (SFTP channels must not be echoed).
    shells: Arc<std::sync::Mutex<Vec<ChannelId>>>,
}

impl server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "test" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        ch: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(ch.id(), ch);
        reply.accept().await;
        Ok(())
    }

    async fn shell_request(&mut self, id: ChannelId, s: &mut Session) -> Result<(), Self::Error> {
        s.channel_success(id)?;
        self.shells.lock().unwrap().push(id);
        s.data(id, &b"dev_sshd: input is echoed\r\n$ "[..])?;
        Ok(())
    }

    async fn data(&mut self, id: ChannelId, data: &[u8], s: &mut Session) -> Result<(), Self::Error> {
        if !self.shells.lock().unwrap().contains(&id) {
            return Ok(());
        }
        let out: Vec<u8> = data
            .iter()
            .flat_map(|&b| if b == b'\r' { b"\r\n$ ".to_vec() } else { vec![b] })
            .collect();
        s.data(id, out)?;
        Ok(())
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, s: &mut Session) -> Result<(), Self::Error> {
        let ch = self.channels.lock().unwrap().remove(&id);
        match (name, ch) {
            ("sftp", Some(ch)) => {
                s.channel_success(id)?;
                let h = Sftp {
                    root: self.root.clone(),
                    files: HashMap::new(),
                    dirs: HashMap::new(),
                    next: 0,
                };
                tokio::spawn(russh_sftp::server::run(ch.into_stream(), h));
            }
            _ => s.channel_failure(id)?,
        }
        Ok(())
    }
}

struct Sftp {
    root: PathBuf,
    files: HashMap<String, tokio::fs::File>,
    dirs: HashMap<String, bool>,
    next: u64,
}

impl Sftp {
    fn real(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en".into(),
    }
}

fn io(e: std::io::Error) -> StatusCode {
    if e.kind() == std::io::ErrorKind::NotFound {
        StatusCode::NoSuchFile
    } else {
        StatusCode::Failure
    }
}

fn attrs(m: &std::fs::Metadata) -> FileAttributes {
    let mut a = FileAttributes::empty();
    a.size = Some(m.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.permissions = Some(m.mode());
        a.mtime = Some(m.mtime() as u32);
    }
    #[cfg(not(unix))]
    {
        a.permissions = Some(if m.is_dir() { 0o040755 } else { 0o100644 });
    }
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
        let p = if path == "." || path.is_empty() {
            "/".to_owned()
        } else {
            path
        };
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
        let f = tokio::fs::OpenOptions::new()
            .read(flags.contains(OpenFlags::READ))
            .write(flags.contains(OpenFlags::WRITE))
            .create(flags.contains(OpenFlags::CREATE))
            .truncate(flags.contains(OpenFlags::TRUNCATE))
            .open(self.real(&path))
            .await
            .map_err(io)?;
        self.next += 1;
        let h = format!("f{}", self.next);
        self.files.insert(h.clone(), f);
        Ok(Handle { id, handle: h })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.files.remove(&handle);
        self.dirs.remove(&handle);
        Ok(ok(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let f = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(io)?;
        let mut buf = vec![0; len as usize];
        let n = f.read(&mut buf).await.map_err(io)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let f = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(io)?;
        f.write_all(&data).await.map_err(io)?;
        Ok(ok(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let m = std::fs::metadata(self.real(&path)).map_err(io)?;
        Ok(Attrs { id, attrs: attrs(&m) })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let m = std::fs::symlink_metadata(self.real(&path)).map_err(io)?;
        Ok(Attrs { id, attrs: attrs(&m) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let f = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let m = f.metadata().await.map_err(io)?;
        Ok(Attrs { id, attrs: attrs(&m) })
    }

    async fn setstat(&mut self, id: u32, path: String, a: FileAttributes) -> Result<Status, Self::Error> {
        #[cfg(unix)]
        if let Some(p) = a.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(self.real(&path), std::fs::Permissions::from_mode(p & 0o7777)).map_err(io)?;
        }
        #[cfg(not(unix))]
        let _ = (path, a);
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
        let files = std::fs::read_dir(self.real(&handle))
            .map_err(io)?
            .flatten()
            .filter_map(|e| Some(File::new(e.file_name().to_string_lossy(), attrs(&e.metadata().ok()?))))
            .collect();
        Ok(Name { id, files })
    }

    async fn mkdir(&mut self, id: u32, path: String, _: FileAttributes) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.real(&path)).map_err(io)?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.real(&path)).map_err(io)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.real(&path)).map_err(io)?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
        std::fs::rename(self.real(&from), self.real(&to)).map_err(io)?;
        Ok(ok(id))
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: dev_sshd <root dir> [port]"));
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(2299);
    let key = PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), Algorithm::Ed25519).expect("key");
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(100),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("dev_sshd on 127.0.0.1:{port}, root {}", root.display());
    while let Ok((stream, _)) = listener.accept().await {
        let h = Ssh {
            root: root.clone(),
            channels: Default::default(),
            shells: Default::default(),
        };
        let cfg = config.clone();
        tokio::spawn(async move {
            if let Ok(r) = server::run_stream(cfg, stream, h).await {
                let _ = r.await;
            }
        });
    }
}
