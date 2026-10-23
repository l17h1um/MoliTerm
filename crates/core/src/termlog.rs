//! Session output logging (SPEC §7.7): plain text (ANSI stripped) or raw,
//! optional timestamp prefix per line, rotation by size.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
};

use alacritty_terminal::vte::{Parser, Perform};

pub struct LogOptions {
    pub raw: bool,
    pub timestamps: bool,
    /// Rotate after this many bytes; 0 = never.
    pub max_bytes: u64,
}

pub struct SessionLog {
    dir: PathBuf,
    opts: LogOptions,
    file: BufWriter<File>,
    written: u64,
    parser: Parser,
    text: Text,
}

#[derive(Default)]
struct Text {
    out: Vec<u8>,
    line_start: bool,
    timestamps: bool,
}

impl Text {
    fn stamp(&mut self) {
        if self.line_start && self.timestamps {
            self.out.extend_from_slice(local_time().as_bytes());
            self.out.push(b' ');
        }
        self.line_start = false;
    }
}

impl Perform for Text {
    fn print(&mut self, c: char) {
        self.stamp();
        let mut b = [0; 4];
        self.out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.stamp();
                self.out.push(b'\n');
                self.line_start = true;
            }
            b'\t' => {
                self.stamp();
                self.out.push(b'\t');
            }
            _ => {}
        }
    }
}

impl SessionLog {
    /// Opens `<dir>/<YYYY-MM-DD_HH-MM-SS>.log`.
    pub fn open(dir: PathBuf, opts: LogOptions) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let file = BufWriter::new(File::create(dir.join(stamp_name()))?);
        Ok(Self {
            text: Text {
                line_start: true,
                timestamps: opts.timestamps,
                ..Default::default()
            },
            dir,
            opts,
            file,
            written: 0,
            parser: Parser::new(),
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let data: &[u8] = if self.opts.raw {
            bytes
        } else {
            self.text.out.clear();
            self.parser.advance(&mut self.text, bytes);
            &self.text.out
        };
        self.file.write_all(data)?;
        self.written += data.len() as u64;
        if self.opts.max_bytes > 0 && self.written >= self.opts.max_bytes {
            self.file.flush()?;
            self.file = BufWriter::new(File::create(self.dir.join(stamp_name()))?);
            self.written = 0;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `2026-10-13_09-41-07.log`.
fn stamp_name() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S.log").to_string()
}

/// Local time as `YYYY-MM-DD HH:MM:SS`.
pub fn local_time() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        let dir = std::env::temp_dir().join(format!("moliterm-log-{}", std::process::id()));
        let mut log = SessionLog::open(
            dir.clone(),
            LogOptions {
                raw: false,
                timestamps: false,
                max_bytes: 0,
            },
        )
        .unwrap();
        log.write(b"\x1b[1;31mred\x1b[0m text\r\nnext\x1b]0;title\x07 line\n")
            .unwrap();
        log.flush().unwrap();
        let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read_to_string(file).unwrap(), "red text\nnext line\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
