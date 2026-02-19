// crates/velocut-ui/src/helpers/format.rs
//
// UI-layer string utilities that don't belong in velocut-core.
//
// Time and duration formatting lives in velocut_core::helpers::time — use
// those for anything involving seconds/frames.  This module holds utilities
// that are purely about rendering strings in the UI (truncation, labels) and
// have no meaning outside of a display context.

/// Truncate `s` to at most `max` *bytes*, returning a `&str` that ends on a
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