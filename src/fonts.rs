//! Embedded editor fonts.
//!
//! Settings → Editor → Font offers a fixed family list; the families whose
//! files are embedded here always resolve, on any machine. The rest of the
//! list (SF Mono, Menlo, Monaco, Courier New) is provided by the OS and is
//! offered only when actually installed, so a pick never silently renders in
//! a substitute font.
//!
//! Only redistributable faces are embedded (SIL OFL / MIT — see
//! `assets/fonts/licenses/`). Fira Code ships no italic upstream, so its
//! italic style resolves through the fallback stack.

use std::borrow::Cow;

use gpui_kit::*;

/// Every embedded face. `add_fonts` reads family/weight/style from each file's
/// name table, so the order here does not matter.
const BUNDLED_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf"),
    include_bytes!("../assets/fonts/FiraCode-Regular.ttf"),
    include_bytes!("../assets/fonts/FiraCode-Bold.ttf"),
    include_bytes!("../assets/fonts/FiraCode-Medium.ttf"),
    include_bytes!("../assets/fonts/FiraCode-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Hack-Regular.ttf"),
    include_bytes!("../assets/fonts/Hack-Bold.ttf"),
    include_bytes!("../assets/fonts/Hack-Italic.ttf"),
    include_bytes!("../assets/fonts/Hack-BoldItalic.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-Regular.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-Bold.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-Italic.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-BoldItalic.ttf"),
];

/// Register the embedded faces with the text system. Call once after
/// `gpui_kit::init` and before the first window lays out text. A failure is
/// logged and skipped so a corrupt asset cannot abort startup.
pub fn register_bundled_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = BUNDLED_FONTS
        .iter()
        .map(|bytes| Cow::Borrowed(*bytes))
        .collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        crate::logging::error(format!("failed to load bundled fonts: {e:#}"));
    }
}

/// Family names this module embeds, in the same spelling as `FONT_FAMILIES`.
/// Used to seed the dropdown and to assert registration in tests.
pub const BUNDLED_FAMILIES: &[&str] = &["JetBrains Mono", "Fira Code", "Cascadia Code", "Hack"];

#[cfg(test)]
mod tests {
    // Named imports only: `use super::*` would drag in `gpui_kit::test`, which
    // shadows the built-in `#[test]` attribute and recurses.
    use super::{BUNDLED_FAMILIES, BUNDLED_FONTS};

    /// Minimal sfnt `name`-table reader (Windows records preferred): returns
    /// `(typographic family else family, subfamily)` for name IDs 16/1, 17/2.
    fn font_names(data: &[u8]) -> (String, String) {
        fn u16b(d: &[u8], o: usize) -> u16 {
            u16::from_be_bytes([d[o], d[o + 1]])
        }
        fn u32b(d: &[u8], o: usize) -> u32 {
            u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
        }
        let num_tables = u16b(data, 4) as usize;
        let (mut off, mut len) = (0usize, 0usize);
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &data[rec..rec + 4] == b"name" {
                off = u32b(data, rec + 8) as usize;
                len = u32b(data, rec + 12) as usize;
            }
        }
        assert!(len > 0, "font has no name table");
        let name = &data[off..off + len];
        let count = u16b(name, 2) as usize;
        let str_base = u16b(name, 4) as usize;
        let (mut family, mut sub) = (None, None);
        let (mut typo_family, mut typo_sub) = (None, None);
        for i in 0..count {
            let rec = 6 + i * 12;
            let platform = u16b(name, rec);
            let name_id = u16b(name, rec + 6);
            let n_len = u16b(name, rec + 8) as usize;
            let n_off = u16b(name, rec + 10) as usize;
            let raw = &name[str_base + n_off..str_base + n_off + n_len];
            let s = if platform == 3 {
                let units: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                String::from_utf16_lossy(&units)
            } else {
                raw.iter().map(|&b| b as char).collect()
            };
            match name_id {
                1 => family = Some(s),
                2 => sub = Some(s),
                16 => typo_family = Some(s),
                17 => typo_sub = Some(s),
                _ => {}
            }
        }
        (
            typo_family.or(family).expect("font missing family name"),
            typo_sub.or(sub).unwrap_or_default(),
        )
    }

    #[test]
    fn every_bundled_family_is_offered() {
        for family in BUNDLED_FAMILIES {
            assert!(
                crate::config::FONT_FAMILIES
                    .iter()
                    .any(|(label, value)| label == family && value == family),
                "bundled family {family:?} missing from FONT_FAMILIES"
            );
        }
    }

    #[test]
    fn embedded_fonts_carry_expected_family_names() {
        let mut families = std::collections::BTreeSet::new();
        for (i, data) in BUNDLED_FONTS.iter().enumerate() {
            assert!(data.len() > 4, "font {i} is empty");
            let (family, sub) = font_names(data);
            assert!(
                BUNDLED_FAMILIES.contains(&family.as_str()),
                "font {i} family {family:?} is not one of {BUNDLED_FAMILIES:?}"
            );
            assert!(!sub.is_empty(), "font {i} ({family}) has no subfamily");
            families.insert(family);
        }
        for expected in BUNDLED_FAMILIES {
            assert!(
                families.contains(*expected),
                "no embedded face for {expected:?}"
            );
        }
    }
}
