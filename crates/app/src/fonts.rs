//! System fonts only (SPEC §4.7). Scanning takes ~0.3 s, so it runs off the UI
//! thread; egui's built-in fonts are used until the result arrives.

use egui::{FontData, FontDefinitions, FontFamily};
use std::sync::{Arc, mpsc};

pub struct Loaded {
    pub defs: FontDefinitions,
    pub db: Arc<fontdb::Database>,
    /// Installed monospace families, sorted.
    pub monospace: Vec<String>,
    /// Family used when the setting is empty.
    pub default_mono: String,
    /// Bold/italic terminal families (`mono-b`, `mono-i`, `mono-bi`) are registered.
    pub styled: bool,
}

#[cfg(target_os = "macos")]
const UI: &[&str] = &["System Font", "Helvetica Neue"];
#[cfg(target_os = "windows")]
const UI: &[&str] = &["Segoe UI"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const UI: &[&str] = &[];

#[cfg(target_os = "macos")]
const MONO: &[&str] = &["Menlo", "Monaco"];
#[cfg(target_os = "windows")]
const MONO: &[&str] = &["Cascadia Mono", "Consolas"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO: &[&str] = &["DejaVu Sans Mono"];

const CJK: &[&str] = &[
    "PingFang SC",
    "Hiragino Sans GB",
    "Microsoft YaHei",
    "Noto Sans CJK SC",
    "WenQuanYi Micro Hei",
];

/// Loads fonts in a background thread. `db` = reuse an earlier scan.
pub fn spawn(ctx: egui::Context, db: Option<Arc<fontdb::Database>>, term_font: String) -> mpsc::Receiver<Loaded> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let db = db.unwrap_or_else(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        });
        let _ = tx.send(build(db, &term_font));
        ctx.request_repaint();
    });
    rx
}

fn family_names(db: &fontdb::Database, mono: bool) -> Vec<String> {
    let mut v: Vec<String> = db
        .faces()
        .filter(|f| f.monospaced == mono)
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn load(db: &fontdb::Database, family: fontdb::Family) -> Option<FontData> {
    load_styled(db, family, false, false)
}

fn load_styled(db: &fontdb::Database, family: fontdb::Family, bold: bool, italic: bool) -> Option<FontData> {
    let id = db.query(&fontdb::Query {
        families: &[family],
        weight: if bold {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        },
        style: if italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
        ..Default::default()
    })?;
    let face = db.face(id)?;
    let mut data = match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => FontData::from_static(mapped(path)?),
        fontdb::Source::Binary(_) => db.with_face_data(id, |bytes, _| FontData::from_owned(bytes.to_vec()))?,
    };
    data.index = face.index;
    Some(data)
}

/// Font files are memory-mapped once and kept for the whole run. Mapped pages
/// are file-backed (paged in only for glyphs actually drawn, not counted as the
/// app's own memory), and egui does not copy borrowed font bytes — loading a
/// 20 MB CJK fallback into the heap cost ~40 MB before.
fn mapped(path: &std::path::Path) -> Option<&'static [u8]> {
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Mutex, OnceLock},
    };
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, &'static [u8]>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    if let Some(bytes) = cache.get(path) {
        return Some(bytes);
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: installed system fonts are not rewritten while the app runs (fontdb maps them the same way).
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    cache.insert(path.to_path_buf(), bytes);
    Some(bytes)
}

fn first(db: &fontdb::Database, names: &[&str], generic: fontdb::Family) -> Option<(String, FontData)> {
    names
        .iter()
        .find_map(|n| load(db, fontdb::Family::Name(n)).map(|d| (n.to_string(), d)))
        .or_else(|| {
            let id = db.query(&fontdb::Query {
                families: &[generic],
                ..Default::default()
            })?;
            let name = db.face(id)?.families.first()?.0.clone();
            load(db, generic).map(|d| (name, d))
        })
}

fn build(db: Arc<fontdb::Database>, term_font: &str) -> Loaded {
    let mut defs = FontDefinitions::default();
    let push_front = |defs: &mut FontDefinitions, fam: FontFamily, key: &str, data: FontData| {
        defs.font_data.insert(key.to_owned(), Arc::new(data));
        defs.families.entry(fam).or_default().insert(0, key.to_owned());
    };

    if let Some(data) = CJK.iter().find_map(|n| load(&db, fontdb::Family::Name(n))) {
        defs.font_data.insert("sys-cjk".into(), Arc::new(data));
        for fam in [FontFamily::Proportional, FontFamily::Monospace] {
            defs.families.entry(fam).or_default().insert(0, "sys-cjk".into());
        }
    }
    if let Some((_, data)) = first(&db, UI, fontdb::Family::SansSerif) {
        push_front(&mut defs, FontFamily::Proportional, "sys-ui", data);
    }
    let default_mono = first(&db, MONO, fontdb::Family::Monospace);
    let default_name = default_mono.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
    let installed = !term_font.is_empty()
        && db
            .query(&fontdb::Query {
                families: &[fontdb::Family::Name(term_font)],
                ..Default::default()
            })
            .is_some();
    let mono_name = if installed {
        term_font.to_owned()
    } else {
        default_name.clone()
    };
    let mut styled = false;
    if let Some(data) = load(&db, fontdb::Family::Name(&mono_name)).or(default_mono.map(|(_, d)| d)) {
        push_front(&mut defs, FontFamily::Monospace, "sys-mono", data);
        let base = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
        for (fam, bold, italic) in [
            ("mono-b", true, false),
            ("mono-i", false, true),
            ("mono-bi", true, true),
        ] {
            let mut list = base.clone();
            if let Some(data) = load_styled(&db, fontdb::Family::Name(&mono_name), bold, italic) {
                let key = format!("sys-{fam}");
                defs.font_data.insert(key.clone(), Arc::new(data));
                list[0] = key;
            }
            defs.families.insert(FontFamily::Name(fam.into()), list);
        }
        styled = true;
    }

    Loaded {
        monospace: family_names(&db, true),
        defs,
        db,
        default_mono: default_name,
        styled,
    }
}
