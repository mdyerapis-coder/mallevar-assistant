//! Canonical citation-locator serialisation. A citation's locator must round-trip
//! to the exact stored chunk it names, so the key is the deterministic serde JSON
//! of the `CitationLocator` (declaration order, `None` fields omitted). Both the
//! writer (ingest) and the opener (CitationResolver) use this same path, so an
//! opened citation resolves to exactly the unit that produced it.

use crate::contracts::CitationLocator;

/// Sentinel locator key that can never collide with a real content unit: a NUL
/// prefix is impossible in any `CitationLocator` serde serialisation. The
/// revision metadata row (mime/extractor identity/disclosure) lives here.
pub const META_LOCATOR: &str = "\u{0}__revision_meta__";

pub fn locator_key(locator: &CitationLocator) -> String {
    serde_json::to_string(locator).unwrap_or_else(|_| META_LOCATOR.to_string())
}

/// Build a locator that names a contiguous 1-based line range.
pub fn lines(start: u32, end: u32) -> CitationLocator {
    CitationLocator { page: None, section: None, line_start: Some(start), line_end: Some(end), sheet: None, cell_range: None, bounding_box: None, time_start_ms: None, time_end_ms: None }
}

/// Build a locator that names a single PDF page.
pub fn page(n: u32) -> CitationLocator {
    CitationLocator { page: Some(n), section: None, line_start: None, line_end: None, sheet: None, cell_range: None, bounding_box: None, time_start_ms: None, time_end_ms: None }
}

/// Build a locator that names an HTML section anchor.
pub fn section(name: &str) -> CitationLocator {
    CitationLocator { page: None, section: Some(name.to_string()), line_start: None, line_end: None, sheet: None, cell_range: None, bounding_box: None, time_start_ms: None, time_end_ms: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locator_key_is_deterministic_and_omits_none() {
        let a = lines(3, 7);
        let b = CitationLocator { page: None, section: None, line_start: Some(3), line_end: Some(7), sheet: None, cell_range: None, bounding_box: None, time_start_ms: None, time_end_ms: None };
        assert_eq!(locator_key(&a), locator_key(&b));
        assert_eq!(locator_key(&a), r#"{"line_start":3,"line_end":7}"#);
        assert_ne!(locator_key(&a), locator_key(&page(1)));
        assert_eq!(locator_key(&page(4)), r#"{"page":4}"#);
        assert!(META_LOCATOR.starts_with('\u{0}'));
    }
}
