//! Debug builds only. `MOLITERM_SHOT=out.bmp` saves a screenshot of the main
//! window shortly after start, then quits. Used to check layouts without a
//! screen-recording permission. `MOLITERM_SHOT_ACTIONS` (comma-separated) is
//! read by `App::new` to put the UI into a given state first.

use std::{path::PathBuf, time::Instant};

pub struct DevShot {
    path: PathBuf,
    start: Instant,
    requested: bool,
    /// `MOLITERM_SHOT_DELAY` seconds (default 2), e.g. for slow remote logins.
    delay: f32,
}

impl DevShot {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            path: std::env::var_os("MOLITERM_SHOT")?.into(),
            start: Instant::now(),
            requested: false,
            delay: std::env::var("MOLITERM_SHOT_DELAY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0),
        })
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        // Wake only when the shot is due, so idle CPU can be measured meanwhile.
        let left = self.delay - self.start.elapsed().as_secs_f32();
        ctx.request_repaint_after(std::time::Duration::from_secs_f32(left.max(0.05)));
        if !self.requested && left <= 0.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.requested = true;
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            if let Err(e) = std::fs::write(&self.path, bmp(&img)) {
                eprintln!("devshot: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// 32-bit top-down BMP.
fn bmp(img: &egui::ColorImage) -> Vec<u8> {
    let [w, h] = img.size;
    let data_len = (w * h * 4) as u32;
    let mut out = Vec::with_capacity(54 + data_len as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + data_len).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for p in &img.pixels {
        out.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
    }
    out
}
