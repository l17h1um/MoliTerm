//! Serial console (SPEC §5.4) on top of the blocking `serialport` crate:
//! one reader and one writer thread per tab.

use std::{
    io::{ErrorKind, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::terminal::{Cmd, SessionEvent, Wire};

pub const BAUDS: [u32; 10] = [
    9600, 19200, 38400, 57600, 115200, 230400, 250000, 460800, 500000, 921600,
];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Flow {
    #[default]
    None,
    RtsCts,
    XonXoff,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LineEnding {
    #[default]
    Cr,
    Lf,
    CrLf,
}

/// Serial settings stored in `Session.extra`; the device path is `Session.host`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Extra {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub flow: Flow,
    pub local_echo: bool,
    pub line_ending: LineEnding,
}

impl Default for Extra {
    fn default() -> Self {
        Self {
            baud: 115200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow: Flow::None,
            local_echo: false,
            line_ending: LineEnding::Cr,
        }
    }
}

impl Extra {
    /// `115200 8N1`
    pub fn summary(&self) -> String {
        let p = match self.parity {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
        };
        format!("{} {}{p}{}", self.baud, self.data_bits, self.stop_bits)
    }

    /// Enter key (CR from the terminal) → configured line ending.
    pub fn translate(&self, bytes: &[u8]) -> Vec<u8> {
        let nl: &[u8] = match self.line_ending {
            LineEnding::Cr => b"\r",
            LineEnding::Lf => b"\n",
            LineEnding::CrLf => b"\r\n",
        };
        let mut v = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if b == b'\r' {
                v.extend_from_slice(nl);
            } else {
                v.push(b);
            }
        }
        v
    }
}

/// Detected ports (`/dev/tty.*`, `/dev/ttyUSB*`, `COMx`).
pub fn ports() -> Vec<String> {
    let mut v: Vec<String> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        // macOS lists each device twice; `cu.*` is the one to open for dial-out.
        .filter(|n| !n.starts_with("/dev/tty."))
        .collect();
    v.sort();
    v
}

fn open(path: &str, x: &Extra) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    // macOS sets the speed with an ioctl that ptys (socat test pairs) reject
    // with ENOTTY; baud 0 skips it and a pty has no speed anyway.
    open_at(path, x, x.baud).or_else(|e| match cfg!(target_os = "macos") {
        true => open_at(path, x, 0).map_err(|_| e),
        false => Err(e),
    })
}

fn open_at(path: &str, x: &Extra, baud: u32) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    use serialport::{DataBits, FlowControl, StopBits};
    serialport::new(path, baud)
        .data_bits(match x.data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        })
        .parity(match x.parity {
            Parity::None => serialport::Parity::None,
            Parity::Odd => serialport::Parity::Odd,
            Parity::Even => serialport::Parity::Even,
        })
        .stop_bits(if x.stop_bits == 2 { StopBits::Two } else { StopBits::One })
        .flow_control(match x.flow {
            Flow::None => FlowControl::None,
            Flow::RtsCts => FlowControl::Hardware,
            Flow::XonXoff => FlowControl::Software,
        })
        .timeout(Duration::from_millis(100))
        .open()
}

/// Opens `path` and attaches it to `wire`; failures end up as `Disconnected`.
pub fn spawn(rt: &tokio::runtime::Handle, path: String, x: Extra, wire: Wire) {
    wire.drain();
    rt.spawn_blocking(move || {
        wire.event(SessionEvent::Info {
            target: path.clone(),
            detail: x.summary(),
        });
        let (mut reader, mut writer) = match open(&path, &x).and_then(|p| Ok((p.try_clone()?, p))) {
            Ok(p) => p,
            Err(e) => return wire.event(SessionEvent::Disconnected(format!("{path}: {e}"))),
        };
        wire.event(SessionEvent::Connected);
        let stop = Arc::new(AtomicBool::new(false));
        let (out, quit) = (wire.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let reason = loop {
                if quit.load(Ordering::Relaxed) {
                    return;
                }
                match reader.read(&mut buf) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(20)),
                    Ok(n) => out.feed(&buf[..n]),
                    Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                    Err(e) => break e.to_string(),
                }
            };
            out.stop();
            out.event(SessionEvent::Disconnected(reason));
        });
        let mut cmds = wire.commands_blocking();
        while let Some(cmd) = cmds.blocking_recv() {
            match cmd {
                Cmd::Input(bytes) => {
                    let out = x.translate(&bytes);
                    if writer.write_all(&out).and_then(|_| writer.flush()).is_err() {
                        break;
                    }
                    if x.local_echo {
                        wire.feed(&bytes.iter().flat_map(|&b| echo(b)).collect::<Vec<u8>>());
                    }
                }
                Cmd::Break => {
                    let _ = writer.set_break();
                    std::thread::sleep(Duration::from_millis(250));
                    let _ = writer.clear_break();
                }
                Cmd::Resize(_) => {}
                Cmd::Close => break,
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
}

/// Local echo: show any line ending as a new line.
fn echo(b: u8) -> Vec<u8> {
    match b {
        b'\r' | b'\n' => b"\r\n".to_vec(),
        0x7f => b"\x08 \x08".to_vec(),
        b => vec![b],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let mut x = Extra::default();
        assert_eq!(x.summary(), "115200 8N1");
        assert_eq!(x.translate(b"ls\r"), b"ls\r");
        x.line_ending = LineEnding::CrLf;
        assert_eq!(x.translate(b"ls\r"), b"ls\r\n");
        let json = serde_json::to_value(&x).unwrap();
        assert_eq!(json["line_ending"], "crlf");
        assert_eq!(serde_json::from_value::<Extra>(json).unwrap(), x);
    }

    /// A pty stands in for the device: what the "device" writes shows up on
    /// screen, what the user types (with the line ending) reaches the device.
    #[cfg(unix)]
    #[test]
    fn pty_as_device() {
        use portable_pty::{PtySize, native_pty_system};
        use std::sync::RwLock;
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let path = pair.master.tty_name().unwrap().to_string_lossy().into_owned();
        let mut dev_out = pair.master.take_writer().unwrap();
        let mut dev_in = pair.master.try_clone_reader().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let colors = Arc::new(RwLock::new(
            crate::theme::load_all(std::path::Path::new("/x"))[0].term.clone(),
        ));
        let size = crate::terminal::Size {
            cols: 40,
            rows: 5,
            cell_w: 8,
            cell_h: 16,
        };
        let (handle, wire) = crate::terminal::pipeline(size, 10, colors, None, None, Arc::new(|| {}));
        let x = Extra {
            line_ending: LineEnding::CrLf,
            ..Default::default()
        };
        spawn(rt.handle(), path, x, wire);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match handle.events.recv_timeout(Duration::from_millis(50)) {
                Ok(SessionEvent::Connected) => break,
                Ok(SessionEvent::Disconnected(e)) => panic!("{e}"),
                _ => {}
            }
            assert!(std::time::Instant::now() < deadline, "port did not open");
        }
        dev_out.write_all(b"U-Boot 2024\r\n").unwrap();
        handle.send(&b"help\r"[..]);
        let mut got = vec![];
        let mut buf = [0u8; 64];
        while !got.ends_with(b"help\r\n") {
            let n = dev_in.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        while !crate::terminal::all_text(&*handle.term.lock()).contains("U-Boot 2024") {
            assert!(std::time::Instant::now() < deadline, "device output not shown");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = handle.cmd.send(Cmd::Close);
    }
}
