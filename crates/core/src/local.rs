//! Local shell sessions over a PTY (SPEC §5.5).

use std::{io::Read, io::Write, path::PathBuf};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::{
    Error,
    terminal::{Cmd, SessionEvent, Size, Wire},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

/// Detected shells, default first. May run external programs (`wsl.exe`), so
/// call it off the UI thread.
pub fn detect() -> Vec<Shell> {
    #[cfg(unix)]
    {
        let login = |p: &str| Shell {
            name: std::path::Path::new(p)
                .file_name()
                .map_or_else(|| p.to_owned(), |n| n.to_string_lossy().into_owned()),
            program: p.to_owned(),
            args: vec!["-l".into()],
        };
        let mut out: Vec<Shell> = std::env::var("SHELL").ok().map(|s| login(&s)).into_iter().collect();
        let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
        for line in listed.lines().map(str::trim) {
            if line.starts_with('/') && std::path::Path::new(line).exists() && !out.iter().any(|s| s.program == line) {
                out.push(login(line));
            }
        }
        if out.is_empty() {
            out.push(login("/bin/sh"));
        }
        out
    }
    #[cfg(windows)]
    {
        let mut out = vec![];
        let plain = |name: &str, program: &str| Shell {
            name: name.into(),
            program: program.into(),
            args: vec![],
        };
        if let Some(pwsh) = which("pwsh.exe") {
            out.push(plain("PowerShell 7", &pwsh));
        }
        out.push(plain("Windows PowerShell", "powershell.exe"));
        out.push(plain("cmd", "cmd.exe"));
        for distro in wsl_distros() {
            out.push(Shell {
                name: format!("WSL: {distro}"),
                program: "wsl.exe".into(),
                args: vec!["-d".into(), distro],
            });
        }
        out
    }
}

#[cfg(windows)]
fn which(exe: &str) -> Option<String> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn wsl_distros() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(out) = std::process::Command::new("wsl.exe")
        .args(["-l", "-q"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return vec![];
    };
    // wsl.exe prints UTF-16LE.
    let units: Vec<u16> = out
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|l| l.trim().trim_matches('\0').to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

fn pty_err(e: impl std::fmt::Display) -> Error {
    Error::Pty(e.to_string())
}

fn pty_size(s: Size) -> PtySize {
    PtySize {
        rows: s.rows,
        cols: s.cols,
        pixel_width: s.cols.saturating_mul(s.cell_w),
        pixel_height: s.rows.saturating_mul(s.cell_h),
    }
}

/// Starts `shell` in a PTY attached to `wire`.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    shell: &Shell,
    cwd: Option<PathBuf>,
    size: Size,
    wire: Wire,
) -> Result<(), Error> {
    wire.drain();
    let pair = native_pty_system().openpty(pty_size(size)).map_err(pty_err)?;
    let mut cmd = CommandBuilder::new(&shell.program);
    cmd.args(&shell.args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "MoliTerm");
    // Apps started from Finder/Explorer have no LANG; shells then mangle UTF-8.
    if std::env::var_os("LANG").is_none() && cfg!(unix) {
        cmd.env("LANG", "en_US.UTF-8");
    }
    if let Some(dir) = cwd.or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())) {
        cmd.cwd(dir);
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(pty_err)?;
    let mut writer = pair.master.take_writer().map_err(pty_err)?;
    let mut killer = child.clone_killer();
    let master = pair.master;

    wire.event(SessionEvent::Connected);
    let out = wire.clone();
    rt.spawn_blocking(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.feed(&buf[..n]),
            }
        }
        let reason = match child.wait() {
            Ok(status) => format!("exit status {}", status.exit_code()),
            Err(e) => e.to_string(),
        };
        out.stop();
        out.event(SessionEvent::Disconnected(reason));
    });
    rt.spawn_blocking(move || {
        let mut cmds = wire.commands_blocking();
        while let Some(cmd) = cmds.blocking_recv() {
            match cmd {
                Cmd::Input(bytes) => {
                    if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                        break;
                    }
                }
                Cmd::Resize(s) => {
                    let _ = master.resize(pty_size(s));
                }
                Cmd::Break => {}
                Cmd::Close => break,
            }
        }
        let _ = killer.kill();
    });
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    #[test]
    fn echo_through_pty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let colors = Arc::new(RwLock::new(
            crate::theme::load_all(std::path::Path::new("/x"))[0].term.clone(),
        ));
        let size = Size {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 16,
        };
        let (handle, wire) = crate::terminal::pipeline(size, 100, colors, None, None, Arc::new(|| {}));
        let shell = Shell {
            name: "sh".into(),
            program: "/bin/sh".into(),
            args: vec![],
        };
        spawn(rt.handle(), &shell, None, size, wire).unwrap();
        handle.send(&b"echo moli$((40+2)); exit\n"[..]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(SessionEvent::Disconnected(_)) = handle.events.recv_timeout(std::time::Duration::from_millis(100))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "shell did not exit");
        }
        assert!(crate::terminal::all_text(&*handle.term.lock()).contains("moli42"));
    }
}
