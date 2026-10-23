//! Remote desktop tab (SPEC §5.8): toolbar + scaled surface + input capture.
//! Talks only to the protocol-agnostic `rdp::{Handle, Frame, Input}` types so a
//! VNC transport can reuse it.

use std::time::{Duration, Instant};

use egui::{Color32, Event, Key, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, vec2};
use moliterm_core::{
    rdp::{self, Input, Scale},
    terminal::SessionEvent,
    theme::Theme,
};

use crate::{
    i18n::I18n,
    style::c,
    term_view::{Req, State},
};

pub struct DeskTab {
    pub id: u64,
    pub name: String,
    pub session_id: Option<String>,
    pub state: State,
    pub info: Option<(String, String)>,
    pub handle: rdp::Handle,
    pub link: rdp::Link,
    pub opts: rdp::Options,
    pub scale: Scale,
    pub color: Option<Color32>,
    /// TLS without NLA.
    pub weak: bool,
    /// Waiting for the first frame to learn the tab size.
    pub pending_start: bool,
    texture: Option<(TextureHandle, u64)>,
    /// Remote size we last asked for, and when the tab size last changed.
    asked: (u16, u16),
    resize_at: Option<Instant>,
    /// Modifier keys currently held on the remote side.
    held: [bool; 4],
    last_mouse: Option<(u16, u16)>,
    wheel: egui::Vec2,
    focused: bool,
}

pub struct Dx<'a> {
    pub i18n: &'a I18n,
    pub theme: &'a Theme,
    /// Cmd → Ctrl on macOS (SPEC §5.8).
    pub mac_keys: bool,
    pub color_border: bool,
    /// Full screen of the tab: no toolbar, the floating bar has the actions.
    pub toolbar: bool,
}

impl DeskTab {
    pub fn new(id: u64, name: String, handle: rdp::Handle, link: rdp::Link, opts: rdp::Options, scale: Scale) -> Self {
        Self {
            id,
            name,
            session_id: None,
            state: State::Connecting,
            info: None,
            handle,
            link,
            opts,
            scale,
            color: None,
            weak: false,
            pending_start: true,
            texture: None,
            asked: (0, 0),
            resize_at: None,
            held: [false; 4],
            last_mouse: None,
            wheel: egui::Vec2::ZERO,
            focused: false,
        }
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Disconnected(_))
    }

    pub fn state_color(&self, theme: &Theme) -> Color32 {
        match self.state {
            State::Connected => c(theme.ui.success),
            State::Connecting => c(theme.ui.warning),
            State::Disconnected(_) => c(theme.ui.danger),
        }
    }

    /// Remote desktop size (0×0 before the first frame).
    pub fn remote_size(&self) -> (u16, u16) {
        self.handle
            .frame
            .lock()
            .map(|f| (f.width, f.height))
            .unwrap_or_default()
    }

    pub fn poll_events(&mut self) {
        for ev in self.handle.events.try_iter() {
            match ev {
                SessionEvent::Connected => self.state = State::Connected,
                SessionEvent::Disconnected(r) => self.state = State::Disconnected(r),
                SessionEvent::Info { target, detail } => self.info = Some((target, detail)),
                SessionEvent::WeakSecurity => self.weak = true,
                _ => {}
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, dx: &Dx) -> Vec<Req> {
        let mut reqs = vec![];
        self.poll_events();
        let t = |k: &str| dx.i18n.t(k);

        // Toolbar.
        if dx.toolbar {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                for (s, key) in [
                    (Scale::Fit, "rdp-fit"),
                    (Scale::Actual, "rdp-actual"),
                    (Scale::Resize, "rdp-resize"),
                ] {
                    if ui.selectable_label(self.scale == s, t(key)).clicked() && self.scale != s {
                        self.scale = s;
                        self.resize_at = Some(Instant::now());
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(!self.is_dead(), egui::Button::new(t("rdp-ctrl-alt-del")))
                    .clicked()
                {
                    self.handle.send(Input::CtrlAltDel);
                }
                if ui.button(t("rdp-fullscreen")).clicked() {
                    reqs.push(Req::Fullscreen);
                }
                if ui.button(t("tab-move-window")).clicked() {
                    reqs.push(Req::MoveToWindow);
                }
                if self.weak {
                    ui.label(RichText::new(t("rdp-weak")).small().color(c(dx.theme.ui.warning)))
                        .on_hover_text(t("rdp-weak-hint"));
                }
                let (w, h) = self.remote_size();
                if w > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{w}×{h}")).small().weak());
                    });
                }
            });
        }

        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let id = ui.id().with(("desk", self.id));
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);

        // Desired remote size = tab size in points (even width, 200..8192).
        let want = (
            ((rect.width() as u16) & !1).clamp(200, 8192),
            (rect.height() as u16).clamp(200, 8192),
        );
        if self.pending_start {
            self.opts.size = want;
            self.asked = want;
            // The size is known now: ask the app to start the session.
            reqs.push(Req::Reconnect);
            return reqs;
        }
        if self.scale == Scale::Resize && want != self.asked && matches!(self.state, State::Connected) {
            match self.resize_at {
                Some(at) if at.elapsed() > Duration::from_millis(400) => {
                    self.handle.send(Input::Resize {
                        width: want.0,
                        height: want.1,
                    });
                    self.asked = want;
                    self.resize_at = None;
                }
                Some(_) => ui.ctx().request_repaint_after(Duration::from_millis(100)),
                None => {
                    self.resize_at = Some(Instant::now());
                    ui.ctx().request_repaint_after(Duration::from_millis(450));
                }
            }
        }

        let img = self.upload(ui);
        let ppp = ui.ctx().pixels_per_point();
        let target = img.map(|(tex, (w, h))| {
            let size = vec2(w as f32, h as f32);
            let draw = match self.scale {
                Scale::Fit => {
                    let k = (rect.width() / size.x).min(rect.height() / size.y);
                    Rect::from_center_size(rect.center(), size * k)
                }
                // One remote pixel = one physical pixel.
                Scale::Actual => Rect::from_min_size(rect.min, size / ppp),
                // Remote size follows the tab in points.
                Scale::Resize => Rect::from_min_size(rect.min, size),
            };
            ui.painter().with_clip_rect(rect).image(
                tex,
                draw,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            (draw, size)
        });

        if resp.clicked() || resp.drag_started() || resp.secondary_clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        if focused != self.focused {
            self.focused = focused;
            if !focused {
                self.held = [false; 4];
                self.handle.send(Input::ReleaseAll);
            }
        }
        if !self.is_dead()
            && let Some((draw, size)) = target
        {
            self.input(ui, &resp, draw, size, focused, dx.mac_keys);
        }

        if dx.color_border
            && let Some(color) = self.color
        {
            ui.painter()
                .rect_stroke(rect.shrink(1.0), 0.0, Stroke::new(2.0, color), egui::StrokeKind::Inside);
        }
        match &self.state {
            State::Disconnected(reason) => {
                ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(190));
                crate::term_view::dead_card(
                    ui,
                    rect,
                    dx.i18n,
                    dx.theme,
                    reason,
                    self.session_id.is_some(),
                    &mut reqs,
                );
            }
            State::Connecting => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    t("rdp-connecting"),
                    egui::FontId::proportional(14.0),
                    c(dx.theme.ui.muted),
                );
            }
            State::Connected => {}
        }
        reqs
    }

    /// Uploads changed pixels; returns the texture and remote size.
    fn upload(&mut self, ui: &egui::Ui) -> Option<(egui::TextureId, (u16, u16))> {
        let Ok(mut f) = self.handle.frame.lock() else {
            return None;
        };
        if f.width == 0 {
            return None;
        }
        let opts = if self.scale == Scale::Fit {
            TextureOptions::LINEAR
        } else {
            TextureOptions::NEAREST
        };
        let (w, h) = (usize::from(f.width), usize::from(f.height));
        match &mut self.texture {
            Some((tex, generation)) if *generation == f.generation => {
                if let Some([x0, y0, x1, y1]) = f.dirty.take() {
                    let (x0, y0, x1, y1) = (usize::from(x0), usize::from(y0), usize::from(x1), usize::from(y1));
                    let mut part = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
                    for y in y0..y1 {
                        part.extend_from_slice(&f.rgba[(y * w + x0) * 4..(y * w + x1) * 4]);
                    }
                    let img = egui::ColorImage::from_rgba_unmultiplied([x1 - x0, y1 - y0], &part);
                    tex.set_partial([x0, y0], img, opts);
                }
            }
            _ => {
                let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &f.rgba);
                self.texture = Some((
                    ui.ctx().load_texture(format!("desk-{}", self.id), img, opts),
                    f.generation,
                ));
                f.dirty = None;
            }
        }
        let size = (f.width, f.height);
        drop(f);
        self.texture.as_ref().map(|(t, _)| (t.id(), size))
    }

    fn input(&mut self, ui: &egui::Ui, resp: &egui::Response, draw: Rect, size: egui::Vec2, focused: bool, mac: bool) {
        let to_remote = |p: Pos2| -> Option<(u16, u16)> {
            let rel = (p - draw.min) / draw.size();
            (0.0..1.0)
                .contains(&rel.x)
                .then_some(())
                .filter(|_| (0.0..1.0).contains(&rel.y))
                .map(|_| ((rel.x * size.x) as u16, (rel.y * size.y) as u16))
        };
        let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));
        let hovered = resp.hovered() || resp.dragged();
        if focused {
            self.sync_mods(mods, mac);
        }
        for ev in events {
            match ev {
                Event::PointerMoved(p) if hovered => {
                    if let Some(xy) = to_remote(p)
                        && Some(xy) != self.last_mouse
                    {
                        self.last_mouse = Some(xy);
                        self.handle.send(Input::MouseMove { x: xy.0, y: xy.1 });
                    }
                }
                Event::PointerButton {
                    pos, button, pressed, ..
                } if hovered || !pressed => {
                    if let Some((x, y)) = to_remote(pos) {
                        self.handle.send(Input::MouseMove { x, y });
                    }
                    let b = match button {
                        egui::PointerButton::Primary => 0,
                        egui::PointerButton::Middle => 1,
                        egui::PointerButton::Secondary => 2,
                        egui::PointerButton::Extra1 => 3,
                        egui::PointerButton::Extra2 => 4,
                    };
                    self.handle.send(Input::Button {
                        button: b,
                        down: pressed,
                    });
                }
                Event::MouseWheel { unit, delta, .. } if hovered => {
                    let per_notch = match unit {
                        egui::MouseWheelUnit::Point => 50.0,
                        egui::MouseWheelUnit::Line => 1.0,
                        egui::MouseWheelUnit::Page => 0.1,
                    };
                    self.wheel += delta / per_notch * 120.0;
                    for (vertical, v) in [(true, &mut self.wheel.y), (false, &mut self.wheel.x)] {
                        let units = v.trunc();
                        if units != 0.0 {
                            *v -= units;
                            // Horizontal: positive = right on the remote side.
                            let units = if vertical { units } else { -units };
                            self.handle.send(Input::Wheel {
                                vertical,
                                units: units.clamp(-32000.0, 32000.0) as i16,
                            });
                        }
                    }
                }
                Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } if focused => {
                    if let Some(code) = scancode(physical_key.unwrap_or(key)) {
                        self.handle.send(Input::Key { code, down: pressed });
                    }
                }
                // egui turns Cmd/Ctrl+C/X/V into these; send the key itself.
                Event::Copy | Event::Cut | Event::Paste(_) if focused => {
                    let code = match ev {
                        Event::Copy => 0x2e,
                        Event::Cut => 0x2d,
                        _ => 0x2f,
                    };
                    self.handle.send(Input::Key { code, down: true });
                    self.handle.send(Input::Key { code, down: false });
                }
                _ => {}
            }
        }
    }

    /// egui reports modifiers as state, not key events: send the changes.
    fn sync_mods(&mut self, m: egui::Modifiers, mac_keys: bool) {
        let cmd = cfg!(target_os = "macos") && m.mac_cmd;
        let want = [m.shift, m.ctrl || (cmd && mac_keys), m.alt, cmd && !mac_keys];
        const CODES: [u16; 4] = [0x2a, 0x1d, 0x38, 0xe05b];
        for i in 0..4 {
            if want[i] != self.held[i] {
                self.held[i] = want[i];
                self.handle.send(Input::Key {
                    code: CODES[i],
                    down: want[i],
                });
            }
        }
    }
}

/// egui key → PC/AT set-1 scancode (`0xE0xx` = extended). Layout-independent:
/// egui passes the physical (US-position) key when it knows it.
pub fn scancode(key: Key) -> Option<u16> {
    use Key::*;
    let letters = "QWERTYUIOP ASDFGHJKL ZXCVBNM";
    let name = key.name();
    if name.len() == 1
        && let Some(ch) = name.chars().next().filter(char::is_ascii_uppercase)
    {
        // Rows start at 0x10, 0x1E, 0x2C.
        let mut code = 0x10u16;
        for c in letters.chars() {
            match c {
                ' ' if code < 0x1e => code = 0x1e,
                ' ' => code = 0x2c,
                c if c == ch => return Some(code),
                _ => code += 1,
            }
        }
    }
    Some(match key {
        Escape => 0x01,
        Num1 | Exclamationmark => 0x02,
        Num2 => 0x03,
        Num3 => 0x04,
        Num4 => 0x05,
        Num5 => 0x06,
        Num6 => 0x07,
        Num7 => 0x08,
        Num8 => 0x09,
        Num9 => 0x0a,
        Num0 => 0x0b,
        Minus => 0x0c,
        Equals | Plus => 0x0d,
        Backspace => 0x0e,
        Tab => 0x0f,
        OpenBracket | OpenCurlyBracket => 0x1a,
        CloseBracket | CloseCurlyBracket => 0x1b,
        Enter => 0x1c,
        Semicolon | Colon => 0x27,
        Quote => 0x28,
        Backtick => 0x29,
        Backslash | Pipe => 0x2b,
        Comma => 0x33,
        Period => 0x34,
        Slash | Questionmark => 0x35,
        Space => 0x39,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        F11 => 0x57,
        F12 => 0x58,
        Insert => 0xe052,
        Delete => 0xe053,
        Home => 0xe047,
        End => 0xe04f,
        PageUp => 0xe049,
        PageDown => 0xe051,
        ArrowUp => 0xe048,
        ArrowDown => 0xe050,
        ArrowLeft => 0xe04b,
        ArrowRight => 0xe04d,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes() {
        assert_eq!(scancode(Key::Q), Some(0x10));
        assert_eq!(scancode(Key::P), Some(0x19));
        assert_eq!(scancode(Key::A), Some(0x1e));
        assert_eq!(scancode(Key::L), Some(0x26));
        assert_eq!(scancode(Key::Z), Some(0x2c));
        assert_eq!(scancode(Key::M), Some(0x32));
        assert_eq!(scancode(Key::C), Some(0x2e));
        assert_eq!(scancode(Key::ArrowLeft), Some(0xe04b));
    }
}
