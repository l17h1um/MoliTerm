//! Line icons painted with egui shapes (no icon font or image assets).

use egui::{Color32, Painter, Pos2, Rect, Stroke, vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Plus,
    Close,
    Prompt,
    Split,
    Broadcast,
    Link,
    Gear,
    Server,
    Folder,
    FolderPlus,
    Lock,
    Warning,
    ArrowUp,
    Home,
    Refresh,
    Upload,
    Download,
    File,
    Play,
    /// A window popping out of another one.
    Window,
}

pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), vec2(s, s));
    // Map unit coordinates (0..1) into the icon square.
    let at = |x: f32, y: f32| -> Pos2 { r.min + vec2(x, y) * s };
    let st = Stroke::new((s / 12.0).max(1.2), color);
    let line = |a: Pos2, b: Pos2| p.line_segment([a, b], st);
    let rect_u = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    match icon {
        Icon::Plus => {
            line(at(0.5, 0.15), at(0.5, 0.85));
            line(at(0.15, 0.5), at(0.85, 0.5));
        }
        Icon::Close => {
            line(at(0.25, 0.25), at(0.75, 0.75));
            line(at(0.75, 0.25), at(0.25, 0.75));
        }
        Icon::Prompt => {
            line(at(0.15, 0.25), at(0.45, 0.5));
            line(at(0.45, 0.5), at(0.15, 0.75));
            line(at(0.5, 0.8), at(0.88, 0.8));
        }
        Icon::Split => {
            p.rect_stroke(rect_u(0.1, 0.15, 0.9, 0.85), s * 0.06, st, egui::StrokeKind::Middle);
            line(at(0.5, 0.15), at(0.5, 0.85));
        }
        Icon::Broadcast => {
            p.circle_filled(at(0.5, 0.5), s * 0.08, color);
            for (rad, sweep) in [(0.22, 0.9), (0.38, 0.8)] {
                for side in [-1.0f32, 1.0] {
                    let pts: Vec<Pos2> = (0..=8)
                        .map(|i| {
                            let a = (i as f32 / 8.0 - 0.5) * sweep * std::f32::consts::PI * 0.6;
                            let (sn, cs) = a.sin_cos();
                            at(0.5 + side * cs * rad, 0.5 + sn * rad)
                        })
                        .collect();
                    p.add(egui::Shape::line(pts, st));
                }
            }
        }
        Icon::Link => {
            let a = rect_u(0.08, 0.32, 0.56, 0.68);
            let b = rect_u(0.44, 0.32, 0.92, 0.68);
            p.rect_stroke(a, s * 0.18, st, egui::StrokeKind::Middle);
            p.rect_stroke(b, s * 0.18, st, egui::StrokeKind::Middle);
        }
        Icon::Gear => {
            p.circle_stroke(at(0.5, 0.5), s * 0.16, st);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (sn, cs) = a.sin_cos();
                line(at(0.5 + cs * 0.26, 0.5 + sn * 0.26), at(0.5 + cs * 0.4, 0.5 + sn * 0.4));
            }
        }
        Icon::Server => {
            p.rect_stroke(rect_u(0.12, 0.15, 0.88, 0.47), s * 0.06, st, egui::StrokeKind::Middle);
            p.rect_stroke(rect_u(0.12, 0.53, 0.88, 0.85), s * 0.06, st, egui::StrokeKind::Middle);
        }
        Icon::Lock => {
            p.rect_stroke(rect_u(0.2, 0.45, 0.8, 0.88), s * 0.06, st, egui::StrokeKind::Middle);
            let arc: Vec<Pos2> = (0..=10)
                .map(|i| {
                    let a = std::f32::consts::PI * (1.0 + i as f32 / 10.0);
                    at(0.5 + a.cos() * 0.2, 0.45 + a.sin() * 0.25)
                })
                .collect();
            p.add(egui::Shape::line(arc, st));
        }
        Icon::Warning => {
            let tri = vec![at(0.5, 0.1), at(0.92, 0.88), at(0.08, 0.88), at(0.5, 0.1)];
            p.add(egui::Shape::line(tri, st));
            line(at(0.5, 0.38), at(0.5, 0.62));
            p.circle_filled(at(0.5, 0.75), s * 0.05, color);
        }
        Icon::ArrowUp => {
            line(at(0.5, 0.85), at(0.5, 0.15));
            line(at(0.22, 0.42), at(0.5, 0.15));
            line(at(0.78, 0.42), at(0.5, 0.15));
        }
        Icon::Home => {
            let roof = vec![at(0.1, 0.5), at(0.5, 0.15), at(0.9, 0.5)];
            p.add(egui::Shape::line(roof, st));
            let body = vec![at(0.22, 0.42), at(0.22, 0.85), at(0.78, 0.85), at(0.78, 0.42)];
            p.add(egui::Shape::line(body, st));
        }
        Icon::Refresh => {
            let arc: Vec<Pos2> = (0..=14)
                .map(|i| {
                    let a = -0.6 + i as f32 / 14.0 * 5.0;
                    at(0.5 + a.cos() * 0.32, 0.5 + a.sin() * 0.32)
                })
                .collect();
            let tip = arc[arc.len() - 1];
            p.add(egui::Shape::line(arc, st));
            p.line_segment([tip, tip + vec2(s * 0.18, -s * 0.02)], st);
            p.line_segment([tip, tip + vec2(s * 0.04, -s * 0.18)], st);
        }
        Icon::Upload | Icon::Download => {
            let (from, to) = if icon == Icon::Upload {
                (0.62, 0.12)
            } else {
                (0.12, 0.62)
            };
            line(at(0.5, from), at(0.5, to));
            let dir = if icon == Icon::Upload { 1.0 } else { -1.0 };
            line(at(0.5, to), at(0.3, to + 0.2 * dir));
            line(at(0.5, to), at(0.7, to + 0.2 * dir));
            line(at(0.15, 0.88), at(0.85, 0.88));
        }
        Icon::Window => {
            p.rect_stroke(rect_u(0.1, 0.3, 0.7, 0.9), s * 0.05, st, egui::StrokeKind::Middle);
            line(at(0.3, 0.1), at(0.9, 0.1));
            line(at(0.9, 0.1), at(0.9, 0.7));
            line(at(0.5, 0.5), at(0.85, 0.15));
        }
        Icon::Play => {
            p.add(egui::Shape::convex_polygon(
                vec![at(0.3, 0.2), at(0.82, 0.5), at(0.3, 0.8)],
                egui::Color32::TRANSPARENT,
                st,
            ));
        }
        Icon::File => {
            let pts = vec![
                at(0.25, 0.1),
                at(0.6, 0.1),
                at(0.78, 0.28),
                at(0.78, 0.9),
                at(0.25, 0.9),
                at(0.25, 0.1),
            ];
            p.add(egui::Shape::line(pts, st));
        }
        Icon::Folder | Icon::FolderPlus => {
            let pts = vec![
                at(0.1, 0.25),
                at(0.4, 0.25),
                at(0.48, 0.35),
                at(0.9, 0.35),
                at(0.9, 0.8),
                at(0.1, 0.8),
                at(0.1, 0.25),
            ];
            p.add(egui::Shape::line(pts, st));
            if icon == Icon::FolderPlus {
                line(at(0.5, 0.45), at(0.5, 0.7));
                line(at(0.38, 0.575), at(0.62, 0.575));
            }
        }
    }
}

/// A clickable icon of `size` points. Shows `tip` on hover.
pub fn button(ui: &mut egui::Ui, icon: Icon, size: f32, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size + 8.0, size + 8.0), egui::Sense::click());
    let v = ui.style().interact(&resp);
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, v.bg_fill);
    }
    paint(ui.painter(), rect.shrink(4.0), icon, v.fg_stroke.color);
    resp.on_hover_text(tip)
}
