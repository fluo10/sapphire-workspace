//! A CJK fallback font from the operating system, so apps ship no font assets.
//!
//! egui's built-in fonts have no CJK glyphs. Rather than bundling a 10–20 MB font, this
//! finds one the OS already has — preferring families with Japanese glyph forms — and
//! registers it behind egui's defaults, so Latin text keeps egui's fonts and CJK falls
//! through to the system font. Nothing is redistributed: the font is read at runtime.

use std::sync::Arc;

/// Families tried in order, Japanese glyph forms first.
#[cfg(target_os = "windows")]
pub const PREFERRED_FAMILIES: &[&str] = &[
    "Yu Gothic UI",
    "Yu Gothic",
    "Meiryo UI",
    "Meiryo",
    "MS Gothic",
];
/// Families tried in order, Japanese glyph forms first.
#[cfg(target_os = "macos")]
pub const PREFERRED_FAMILIES: &[&str] = &[
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Hiragino Kaku Gothic Pro",
];
/// Families tried in order, Japanese glyph forms first.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const PREFERRED_FAMILIES: &[&str] = &[
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "Source Han Sans JP",
    "IPAexGothic",
    "IPAGothic",
    "Takao Gothic",
    "VL Gothic",
];

/// Glyphs a usable fallback must have.
const PROBE: [char; 2] = ['あ', '漢'];

/// The key the font is registered under in egui's font definitions.
const FONT_KEY: &str = "system-cjk";

/// A font file found on this system.
#[derive(Clone)]
pub struct SystemFont {
    /// Its family name.
    pub family: String,
    /// The whole font file (a collection keeps all its faces).
    pub data: Vec<u8>,
    /// Which face of the file to use.
    pub index: u32,
}

impl std::fmt::Debug for SystemFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemFont")
            .field("family", &self.family)
            .field("bytes", &self.data.len())
            .field("index", &self.index)
            .finish()
    }
}

/// One face, reduced to what [`pick`] decides on.
struct Candidate {
    id_index: usize,
    families: Vec<String>,
    covers_cjk: bool,
}

/// The first preferred family (in preference order) whose face covers CJK; failing that,
/// any face that does.
fn pick<'a>(faces: &'a [Candidate], preferred: &[&str]) -> Option<&'a Candidate> {
    preferred
        .iter()
        .find_map(|want| {
            faces
                .iter()
                .find(|c| c.covers_cjk && c.families.iter().any(|f| f.eq_ignore_ascii_case(want)))
        })
        .or_else(|| faces.iter().find(|c| c.covers_cjk))
}

/// Find a CJK-capable system font, or `None` when this system has none.
///
/// Scans the OS font directories (on Linux, the ones fontconfig's configuration names) —
/// tens of milliseconds — and reads the chosen file into memory.
pub fn system_cjk_font() -> Option<SystemFont> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let faces: Vec<&fontdb::FaceInfo> = db.faces().collect();

    // Coverage is checked only for faces worth considering: preferred families first, and
    // the whole database only when none of them is present.
    let is_preferred = |f: &fontdb::FaceInfo| {
        f.families.iter().any(|(name, _)| {
            PREFERRED_FAMILIES
                .iter()
                .any(|p| p.eq_ignore_ascii_case(name))
        })
    };
    let any_preferred = faces.iter().any(|f| is_preferred(f));
    let candidates: Vec<Candidate> = faces
        .iter()
        .enumerate()
        .filter(|(_, f)| !any_preferred || is_preferred(f))
        .map(|(i, f)| Candidate {
            id_index: i,
            families: f.families.iter().map(|(n, _)| n.clone()).collect(),
            covers_cjk: db.with_face_data(f.id, covers_cjk).unwrap_or(false),
        })
        .collect();

    let chosen = faces[pick(&candidates, PREFERRED_FAMILIES)?.id_index];
    let family = chosen
        .families
        .first()
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    db.with_face_data(chosen.id, |data, index| SystemFont {
        family,
        data: data.to_vec(),
        index,
    })
}

/// Whether face `index` of `data` has every [`PROBE`] glyph.
fn covers_cjk(data: &[u8], index: u32) -> bool {
    ttf_parser::Face::parse(data, index)
        .map(|face| PROBE.iter().all(|c| face.glyph_index(*c).is_some()))
        .unwrap_or(false)
}

/// Register a system CJK font as a fallback behind egui's defaults, for both the
/// proportional and the monospace family. Returns the family used, or `None` (logged)
/// when the system has none — CJK text then renders as boxes, nothing fails.
pub fn install_system_cjk_fallback(ctx: &egui::Context) -> Option<String> {
    let Some(font) = system_cjk_font() else {
        tracing::warn!("no CJK-capable system font found; CJK text will not render");
        return None;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(font.data);
    data.index = font.index;
    fonts.font_data.insert(FONT_KEY.to_owned(), Arc::new(data));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(FONT_KEY.to_owned());
    }
    ctx.set_fonts(fonts);
    tracing::info!(family = %font.family, "using a system CJK font");
    Some(font.family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(i: usize, fam: &str, covers: bool) -> Candidate {
        Candidate {
            id_index: i,
            families: vec![fam.to_owned()],
            covers_cjk: covers,
        }
    }

    #[test]
    fn the_earliest_preferred_family_wins_regardless_of_database_order() {
        let faces = vec![cand(0, "Meiryo", true), cand(1, "Yu Gothic UI", true)];
        let got = pick(&faces, &["Yu Gothic UI", "Meiryo"]).unwrap();
        assert_eq!(got.id_index, 1);
    }

    #[test]
    fn family_names_match_case_insensitively() {
        let faces = vec![cand(0, "hiragino sans", true)];
        assert!(pick(&faces, &["Hiragino Sans"]).is_some());
    }

    #[test]
    fn a_preferred_family_without_cjk_glyphs_is_skipped() {
        let faces = vec![cand(0, "Yu Gothic UI", false), cand(1, "Meiryo", true)];
        assert_eq!(
            pick(&faces, &["Yu Gothic UI", "Meiryo"]).unwrap().id_index,
            1
        );
    }

    #[test]
    fn with_no_preferred_family_any_cjk_face_is_the_fallback() {
        let faces = vec![
            cand(0, "DejaVu Sans", false),
            cand(1, "Some CJK Font", true),
        ];
        assert_eq!(pick(&faces, &["Noto Sans CJK JP"]).unwrap().id_index, 1);
    }

    #[test]
    fn nothing_covering_cjk_is_none() {
        let faces = vec![cand(0, "DejaVu Sans", false)];
        assert!(pick(&faces, &["Noto Sans CJK JP"]).is_none());
    }

    /// Every supported desktop OS ships at least one of its preferred families; Windows CI
    /// always has Yu Gothic or Meiryo. macOS/Linux are checked by hand (Task 13).
    #[cfg(windows)]
    #[test]
    fn windows_finds_a_system_cjk_font() {
        let start = std::time::Instant::now();
        let font = system_cjk_font().expect("Windows ships Yu Gothic / Meiryo");
        eprintln!("FONT {:?} in {:?}", font, start.elapsed());
        assert!(!font.data.is_empty());
    }
}
