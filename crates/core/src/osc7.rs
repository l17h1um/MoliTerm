//! OSC 7 (`ESC ] 7 ; file://host/path BEL|ST`) scanner. alacritty_terminal
//! ignores OSC 7, so the raw byte stream is scanned separately. Works across
//! chunk boundaries.

#[derive(Default)]
pub struct Osc7 {
    state: State,
    buf: Vec<u8>,
}

#[derive(Default, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    /// Inside `ESC ]`, collecting until BEL or ST.
    Osc,
    OscEsc,
}

const MAX: usize = 4096;

impl Osc7 {
    /// Feeds bytes; returns the last complete cwd seen in them.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut found = None;
        for &b in bytes {
            match (&self.state, b) {
                (State::Ground, 0x1b) => self.state = State::Esc,
                (State::Ground, _) => {}
                (State::Esc, b']') => {
                    self.state = State::Osc;
                    self.buf.clear();
                }
                (State::Esc, 0x1b) => {}
                (State::Esc, _) => self.state = State::Ground,
                (State::Osc, 0x07) => {
                    found = parse(&self.buf).or(found);
                    self.state = State::Ground;
                }
                (State::Osc, 0x1b) => self.state = State::OscEsc,
                (State::Osc, _) => {
                    if self.buf.len() < MAX {
                        self.buf.push(b);
                    } else {
                        self.state = State::Ground;
                    }
                }
                (State::OscEsc, b'\\') => {
                    found = parse(&self.buf).or(found);
                    self.state = State::Ground;
                }
                (State::OscEsc, _) => self.state = State::Esc,
            }
        }
        found
    }
}

/// `7;file://host/some%20path` → `/some path`.
fn parse(osc: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(osc.strip_prefix(b"7;")?).ok()?;
    let rest = s.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    Some(percent_decode(path))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bel_st_split_and_escapes() {
        let mut p = Osc7::default();
        assert_eq!(
            p.feed(b"x\x1b]7;file://web-01/etc/nginx\x07y").as_deref(),
            Some("/etc/nginx")
        );
        assert_eq!(p.feed(b"\x1b]7;file://h/a%20b"), None);
        assert_eq!(p.feed(b"/c\x1b\\").as_deref(), Some("/a b/c"));
        assert_eq!(p.feed(b"\x1b]0;title\x07"), None);
        assert_eq!(p.feed(b"\x1b]7;file:///tmp\x07").as_deref(), Some("/tmp"));
        assert_eq!(p.feed(b"\x1b]7;http://x/y\x07"), None);
    }
}
