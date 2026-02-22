// crates/velocut-ui/src/helpers/format.rs
//
// UI-layer string utilities that don't belong in velocut-core.
//
// Time and duration formatting lives in velocut_core::helpers::time — use
// those for anything involving seconds/frames.  This module holds utilities
// that are purely about rendering strings in the UI (truncation, labels) and
// have no meaning outside of a display context.

/// Truncates `text` to fit within `max_px` using a per-character width
/// heuristic (11px proportional ≈ 6.5 px/char average). Appends "…" when
/// truncated. Avoids egui font measurement, which requires `&mut Fonts`.
///
/// Used by timeline clip labels and anywhere else a pixel-budget string
/// truncation is needed without access to a live `Fonts` instance.
pub fn fit_label(text: &str, max_px: f32) -> String {
    const AVG_CHAR_PX: f32 = 6.5;
    const ELLIPSIS: &str = "…";
    let max_chars = (max_px / AVG_CHAR_PX).max(0.0) as usize;
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    // Reserve one slot for the ellipsis character itself.
    let keep = max_chars.saturating_sub(1);
    text.chars().take(keep).collect::<String>() + ELLIPSIS
}

#[cfg(test)]
mod fit_label_tests {
    use super::*;

    #[test]
    fn short_text_unchanged() {
        assert_eq!(fit_label("hello", 200.0), "hello");
    }

    #[test]
    fn zero_budget_returns_empty() {
        assert_eq!(fit_label("hello", 0.0), "");
    }

    #[test]
    fn truncated_text_has_ellipsis() {
        let result = fit_label("hello world long name", 30.0);
        assert!(result.ends_with('…'));
        assert!(result.len() < "hello world long name".len());
    }
}
/// valid UTF-8 character boundary.
///
/// Used by the library card grid to keep clip names from overflowing their
/// fixed-width tiles.
///
/// # Panics
/// Never — if `max >= s.len()` the original slice is returned unchanged.
///
/// # Note on units
/// `max` is a *byte* count, not a character count.  For ASCII names (the
/// common case) the two are equivalent.  For multibyte characters the
/// returned slice may be shorter than `max` characters; it will never split
/// a codepoint.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk character boundaries until we exceed `max`, then step back one.
    s.char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max)
        .last()
        .map(|i| &s[..i])
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_is_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5),  "hello");
    }

    #[test]
    fn long_ascii_is_clipped() {
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn empty_input() {
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn multibyte_does_not_split_codepoint() {
        // "é" is two bytes (0xC3 0xA9). max=1 must not split it.
        let s = "élan";
        let t = truncate(s, 1);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert!(t.is_empty() || t == "é" || t.len() <= 1);
    }
}