//! Remote files over the SSH connection of a tab (SPEC §6): the `sftp`
//! subsystem, or SCP over exec channels when the server has no SFTP (§5.2).

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use russh::ChannelMsg;
use russh_sftp::{
    client::SftpSession,
    protocol::{FileAttributes, OpenFlags},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::ssh::SshHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// Unix mode bits (permissions only).
    pub perms: u32,
    /// Seconds since the epoch.
    pub mtime: Option<i64>,
}

pub enum Remote {
    Sftp(SftpSession),
    /// No sftp subsystem: transfers via `scp -f/-t`, listing via `ls -la`.
    Scp(SshHandle),
}

type R<T> = Result<T, String>;

fn e(err: impl std::fmt::Display) -> String {
    err.to_string()
}

/// `'it'\''s'` — safe for a POSIX shell.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

pub fn parent(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].into(),
    }
}

/// Runs `cmd` on an exec channel; returns (stdout, exit status).
pub async fn exec(h: &SshHandle, cmd: &str) -> R<(Vec<u8>, u32)> {
    let mut ch = h.channel_open_session().await.map_err(e)?;
    ch.exec(true, cmd).await.map_err(e)?;
    let (mut out, mut status) = (Vec::new(), 0);
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = exit_status,
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok((out, status))
}

/// Output of a command run on an exec channel.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// `None` if the server sent no exit status (e.g. killed by a signal).
    pub status: Option<u32>,
}

/// Runs `cmd` on its own exec channel, keeping stdout and stderr apart (multi-exec jobs).
pub async fn exec_full(h: &SshHandle, cmd: &str) -> R<ExecResult> {
    let mut ch = h.channel_open_session().await.map_err(e)?;
    ch.exec(true, cmd).await.map_err(e)?;
    let (mut out, mut err, mut status) = (Vec::new(), Vec::new(), None);
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        status,
    })
}

async fn exec_ok(h: &SshHandle, cmd: &str) -> R<Vec<u8>> {
    let (out, status) = exec(h, &format!("{cmd} 2>&1")).await?;
    if status == 0 {
        Ok(out)
    } else {
        Err(String::from_utf8_lossy(&out).trim().to_owned())
    }
}

fn perms_from(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut m = 0;
    for (i, bit) in [0o400, 0o200, 0o100, 0o40, 0o20, 0o10, 0o4, 0o2, 0o1]
        .iter()
        .enumerate()
    {
        if b.get(i + 1).is_some_and(|c| !matches!(c, b'-' | b'S' | b'T')) {
            m |= bit;
        }
    }
    m
}

/// Parses `ls -la` (GNU with `--time-style=+%s`, or busybox/BSD dates).
pub fn parse_ls(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || !matches!(f[0].chars().next(), Some('d' | '-' | 'l' | 'c' | 'b' | 'p' | 's')) {
                return None;
            }
            let size = f[4].parse().ok()?;
            let (mtime, name_at) = match f[5].parse::<i64>() {
                Ok(t) if f.len() >= 7 => (Some(t), 6),
                _ if f.len() >= 9 => (None, 8),
                _ => return None,
            };
            // Re-find the name in the original line so spaces survive.
            let mut rest = line;
            for field in &f[..name_at] {
                rest = rest[rest.find(field)? + field.len()..].trim_start();
            }
            let name = rest.split(" -> ").next()?.to_owned();
            if name == "." || name == ".." {
                return None;
            }
            Some(Entry {
                is_dir: f[0].starts_with('d'),
                is_link: f[0].starts_with('l'),
                size,
                perms: perms_from(f[0]),
                mtime,
                name,
            })
        })
        .collect()
}

impl Remote {
    /// Opens SFTP on the connection, falling back to SCP.
    pub async fn open(h: &SshHandle) -> R<Self> {
        let mut ch = h.channel_open_session().await.map_err(e)?;
        ch.request_subsystem(true, "sftp").await.map_err(e)?;
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => return Ok(Remote::Scp(h.clone())),
                _ => {}
            }
        }
        SftpSession::new(ch.into_stream()).await.map(Remote::Sftp).map_err(e)
    }

    pub fn is_scp(&self) -> bool {
        matches!(self, Remote::Scp(_))
    }

    pub async fn home(&self) -> R<String> {
        match self {
            Remote::Sftp(s) => s.canonicalize(".").await.map_err(e),
            Remote::Scp(h) => Ok(String::from_utf8_lossy(&exec_ok(h, "pwd").await?).trim().to_owned()),
        }
    }

    pub async fn list(&self, path: &str) -> R<Vec<Entry>> {
        match self {
            Remote::Sftp(s) => {
                let mut out = vec![];
                for d in s.read_dir(path).await.map_err(e)? {
                    let m = d.metadata();
                    let mut is_dir = m.is_dir();
                    if m.is_symlink() {
                        // Follow links so linked directories can be entered.
                        is_dir = s.metadata(join(path, &d.file_name())).await.is_ok_and(|t| t.is_dir());
                    }
                    out.push(Entry {
                        name: d.file_name(),
                        is_dir,
                        is_link: m.is_symlink(),
                        size: m.size.unwrap_or(0),
                        perms: m.permissions.unwrap_or(0) & 0o7777,
                        mtime: m.mtime.map(i64::from),
                    });
                }
                Ok(out)
            }
            Remote::Scp(h) => {
                let q = quote(path);
                let cmd = format!("LC_ALL=C ls -la --time-style=+%s {q} 2>/dev/null || LC_ALL=C ls -la {q}");
                Ok(parse_ls(&String::from_utf8_lossy(&exec_ok(h, &cmd).await?)))
            }
        }
    }

    /// Size of a remote file, `None` if missing.
    pub async fn size(&self, path: &str) -> Option<u64> {
        match self {
            Remote::Sftp(s) => s.metadata(path).await.ok().and_then(|m| m.size),
            Remote::Scp(h) => {
                let out = exec_ok(h, &format!("wc -c < {}", quote(path))).await.ok()?;
                String::from_utf8_lossy(&out).trim().parse().ok()
            }
        }
    }

    pub async fn mkdir(&self, path: &str) -> R<()> {
        match self {
            Remote::Sftp(s) => s.create_dir(path).await.map_err(e),
            Remote::Scp(h) => exec_ok(h, &format!("mkdir {}", quote(path))).await.map(drop),
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> R<()> {
        match self {
            Remote::Sftp(s) => s.rename(from, to).await.map_err(e),
            Remote::Scp(h) => exec_ok(h, &format!("mv {} {}", quote(from), quote(to))).await.map(drop),
        }
    }

    /// Deletes a file, or a directory with everything inside.
    pub async fn remove(&self, path: &str, is_dir: bool) -> R<()> {
        match self {
            Remote::Sftp(s) if !is_dir => s.remove_file(path).await.map_err(e),
            Remote::Sftp(s) => {
                for d in self.list(path).await? {
                    Box::pin(self.remove(&join(path, &d.name), d.is_dir && !d.is_link)).await?;
                }
                s.remove_dir(path).await.map_err(e)
            }
            Remote::Scp(h) => exec_ok(h, &format!("rm -rf {}", quote(path))).await.map(drop),
        }
    }

    pub async fn chmod(&self, path: &str, mode: u32) -> R<()> {
        match self {
            Remote::Sftp(s) => {
                let attrs = FileAttributes {
                    permissions: Some(mode),
                    ..FileAttributes::empty()
                };
                s.set_metadata(path, attrs).await.map_err(e)
            }
            Remote::Scp(h) => exec_ok(h, &format!("chmod {:o} {}", mode, quote(path))).await.map(drop),
        }
    }

    pub async fn mtime(&self, path: &str) -> Option<i64> {
        match self {
            Remote::Sftp(s) => s.metadata(path).await.ok()?.mtime.map(i64::from),
            Remote::Scp(_) => None,
        }
    }

    /// Downloads into `local` starting at `offset` (appends when > 0).
    pub async fn download(
        &self,
        remote: &str,
        local: &Path,
        offset: u64,
        done: &AtomicU64,
        cancel: &AtomicBool,
    ) -> R<()> {
        let mut out = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(local)
                .await
                .map_err(e)?
        } else {
            tokio::fs::File::create(local).await.map_err(e)?
        };
        done.store(offset, Ordering::Relaxed);
        match self {
            Remote::Sftp(s) => {
                let mut f = s.open(remote).await.map_err(e)?;
                if offset > 0 {
                    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(e)?;
                }
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".into());
                    }
                    let n = f.read(&mut buf).await.map_err(e)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n]).await.map_err(e)?;
                    done.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            Remote::Scp(h) => scp_download(h, remote, &mut out, done, cancel).await?,
        }
        out.flush().await.map_err(e)
    }

    /// Uploads `local` to `remote` starting at `offset`.
    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        offset: u64,
        done: &AtomicU64,
        cancel: &AtomicBool,
    ) -> R<()> {
        let mut src = tokio::fs::File::open(local).await.map_err(e)?;
        done.store(offset, Ordering::Relaxed);
        match self {
            Remote::Sftp(s) => {
                let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
                if offset == 0 {
                    flags |= OpenFlags::TRUNCATE;
                }
                let mut f = s.open_with_flags(remote, flags).await.map_err(e)?;
                if offset > 0 {
                    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(e)?;
                    src.seek(std::io::SeekFrom::Start(offset)).await.map_err(e)?;
                }
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".into());
                    }
                    let n = src.read(&mut buf).await.map_err(e)?;
                    if n == 0 {
                        break;
                    }
                    f.write_all(&buf[..n]).await.map_err(e)?;
                    done.fetch_add(n as u64, Ordering::Relaxed);
                }
                f.shutdown().await.map_err(e)
            }
            Remote::Scp(h) => scp_upload(h, &mut src, remote, done, cancel).await,
        }
    }
}

async fn scp_download(
    h: &SshHandle,
    remote: &str,
    out: &mut tokio::fs::File,
    done: &AtomicU64,
    cancel: &AtomicBool,
) -> R<()> {
    let ch = h.channel_open_session().await.map_err(e)?;
    ch.exec(true, format!("scp -f -- {}", quote(remote))).await.map_err(e)?;
    let mut s = ch.into_stream();
    s.write_all(&[0]).await.map_err(e)?;
    // Header: C<mode> <size> <name>\n
    let mut header = Vec::new();
    loop {
        let b = s.read_u8().await.map_err(e)?;
        if b == b'\n' {
            break;
        }
        header.push(b);
        if header.len() > 4096 {
            return Err("bad scp header".into());
        }
    }
    let header = String::from_utf8_lossy(&header).into_owned();
    if !header.starts_with('C') {
        return Err(header.trim_start_matches(['\x01', '\x02']).to_owned());
    }
    let size: u64 = header
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .ok_or("bad scp header")?;
    s.write_all(&[0]).await.map_err(e)?;
    let mut left = size;
    let mut buf = vec![0u8; 256 * 1024];
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = s.read(&mut buf[..left.min(256 * 1024) as usize]).await.map_err(e)?;
        if n == 0 {
            return Err("connection closed".into());
        }
        out.write_all(&buf[..n]).await.map_err(e)?;
        done.fetch_add(n as u64, Ordering::Relaxed);
        left -= n as u64;
    }
    let _ = s.read_u8().await;
    let _ = s.write_all(&[0]).await;
    Ok(())
}

async fn scp_upload(
    h: &SshHandle,
    src: &mut tokio::fs::File,
    remote: &str,
    done: &AtomicU64,
    cancel: &AtomicBool,
) -> R<()> {
    let size = src.metadata().await.map_err(e)?.len();
    let ch = h.channel_open_session().await.map_err(e)?;
    ch.exec(true, format!("scp -t -- {}", quote(remote))).await.map_err(e)?;
    let mut s = ch.into_stream();
    let ack = async |s: &mut russh::ChannelStream<russh::client::Msg>| -> R<()> {
        match s.read_u8().await.map_err(e)? {
            0 => Ok(()),
            _ => Err("scp refused the transfer".into()),
        }
    };
    ack(&mut s).await?;
    let name = remote.rsplit('/').next().unwrap_or(remote);
    s.write_all(format!("C0644 {size} {name}\n").as_bytes())
        .await
        .map_err(e)?;
    ack(&mut s).await?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = src.read(&mut buf).await.map_err(e)?;
        if n == 0 {
            break;
        }
        s.write_all(&buf[..n]).await.map_err(e)?;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    s.write_all(&[0]).await.map_err(e)?;
    ack(&mut s).await?;
    let _ = s.shutdown().await;
    Ok(())
}

/// Live handle to a tab's remote file access.
pub type Shared = Arc<Remote>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_gnu_busybox_and_paths() {
        let gnu = "total 8\n\
drwxr-xr-x  2 root root 4096 1700000000 .\n\
drwxr-xr-x  3 root root 4096 1700000000 ..\n\
-rw-r-----  1 root root   88 1700000001 .htpasswd\n\
drwxr-xr-x  2 root root 4096 1700000002 conf.d\n\
lrwxrwxrwx  1 root root   10 1700000003 my file -> target\n";
        let e = parse_ls(gnu);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].name, ".htpasswd");
        assert_eq!(e[0].perms, 0o640);
        assert_eq!(e[0].mtime, Some(1_700_000_001));
        assert!(e[1].is_dir);
        assert_eq!(e[2].name, "my file");
        assert!(e[2].is_link);
        let bb = "-rwsr-xr-x    1 root     root        123 Oct 13 09:41 su\n";
        let e = parse_ls(bb);
        assert_eq!((e[0].name.as_str(), e[0].size, e[0].perms), ("su", 123, 0o755));
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(parent("/etc/nginx/"), "/etc");
        assert_eq!(parent("/etc"), "/");
        assert_eq!(join("/", "etc"), "/etc");
    }
}
