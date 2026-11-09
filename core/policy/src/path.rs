//! Canonical absolute *virtual* paths.
//!
//! These paths name locations inside a [`crate::ResourceView`]'s private,
//! in-memory tree. They never name host filesystem locations, and nothing in
//! this crate maps a [`VirtualPath`] onto the host OS.
//!
//! Canonical form, enforced by [`VirtualPath::parse`]:
//!
//! * absolute: must begin with `/`;
//! * no empty segments (so no `//`), no trailing `/` except for the root `/`;
//! * no `.` or `..` segments — traversal is rejected, never resolved;
//! * no backslash, NUL, or ASCII control characters in a segment;
//! * segments are at most [`MAX_SEGMENT_LEN`] bytes and paths at most
//!   [`MAX_PATH_LEN`] bytes.
//!
//! Subtree checks are performed segment-wise, so `/a/bc` is never considered
//! inside `/a/b` (the classic string-prefix escape).

use std::fmt;

use crate::error::{Error, Result};

/// Longest permitted single path segment, in bytes.
pub const MAX_SEGMENT_LEN: usize = 255;
/// Longest permitted whole path, in bytes.
pub const MAX_PATH_LEN: usize = 4096;

/// A canonical absolute virtual path.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtualPath {
    /// Rendered canonical form, always starting with `/`.
    text: String,
    /// Segment boundaries stored as `(offset, len)` into `text`, without the
    /// leading slash. Kept private so the invariant cannot be broken.
    segments: Vec<(u32, u32)>,
}

impl VirtualPath {
    /// The virtual root `/`.
    #[must_use]
    pub fn root() -> VirtualPath {
        VirtualPath {
            text: "/".to_string(),
            segments: Vec::new(),
        }
    }

    /// Parse and canonicalize-check an absolute virtual path.
    ///
    /// Returns [`crate::ErrorKind::InvalidPath`] for relative paths,
    /// traversal, empty segments, over-long paths and malformed segments.
    pub fn parse(raw: &str) -> Result<VirtualPath> {
        if raw.is_empty() || raw.len() > MAX_PATH_LEN {
            return Err(Error::invalid_path("virtual path is empty or too long"));
        }
        if !raw.starts_with('/') {
            return Err(Error::invalid_path("virtual path must be absolute"));
        }
        if raw == "/" {
            return Ok(VirtualPath::root());
        }
        if raw.ends_with('/') {
            return Err(Error::invalid_path("virtual path must not end with '/'"));
        }
        if raw.contains('\0') {
            return Err(Error::invalid_path("virtual path contains NUL"));
        }

        let mut segments = Vec::new();
        let mut offset = 1usize;
        for seg in raw[1..].split('/') {
            if seg.is_empty() {
                return Err(Error::invalid_path("virtual path has an empty segment"));
            }
            if seg == "." || seg == ".." {
                return Err(Error::invalid_path("virtual path traversal is not allowed"));
            }
            if seg.len() > MAX_SEGMENT_LEN {
                return Err(Error::invalid_path("virtual path segment too long"));
            }
            if seg.chars().any(|c| c == '\\' || c.is_control()) {
                return Err(Error::invalid_path("virtual path segment is malformed"));
            }
            // `offset..offset+seg.len()` indexes into `raw`; '/' is ASCII so
            // these boundaries are always valid UTF-8 boundaries here.
            segments.push((
                u32::try_from(offset).map_err(|_| Error::invalid_path("virtual path too long"))?,
                u32::try_from(seg.len()).map_err(|_| Error::invalid_path("segment too long"))?,
            ));
            offset += seg.len() + 1;
        }

        Ok(VirtualPath {
            text: raw.to_string(),
            segments,
        })
    }

    /// Whether this is the root path.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Number of segments.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    /// Segment iterator.
    pub fn segments(&self) -> impl Iterator<Item = &str> + '_ {
        self.segments
            .iter()
            .map(move |&(off, len)| &self.text[off as usize..(off + len) as usize])
    }

    /// Final segment, or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.segments().last()
    }

    /// Parent path, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<VirtualPath> {
        if self.is_root() {
            return None;
        }
        let mut parent = VirtualPath::root();
        let mut prefix = String::new();
        let n = self.segments.len();
        for seg in self.segments().take(n - 1) {
            prefix.push('/');
            prefix.push_str(seg);
            parent.segments.push({
                let off = (prefix.len() - seg.len()) as u32;
                (off, seg.len() as u32)
            });
        }
        parent.text = if prefix.is_empty() {
            "/".to_string()
        } else {
            prefix
        };
        Some(parent)
    }

    /// Append a single validated segment.
    pub fn join_segment(&self, segment: &str) -> Result<VirtualPath> {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('/')
            || segment.contains('\\')
            || segment.contains('\0')
            || segment.len() > MAX_SEGMENT_LEN
            || segment.chars().any(char::is_control)
        {
            return Err(Error::invalid_path("malformed path segment"));
        }
        let mut joined = self.text.clone();
        if joined != "/" {
            joined.push('/');
        }
        joined.push_str(segment);
        VirtualPath::parse(&joined)
    }

    /// Whether `self` is the same as or an ancestor of `other`, compared by
    /// whole segments. `/a/b` contains `/a/b` and `/a/b/c`, never `/a/bc`.
    #[must_use]
    pub fn is_ancestor_or_equal(&self, other: &VirtualPath) -> bool {
        if self.segments.len() > other.segments.len() {
            return false;
        }
        self.segments().zip(other.segments()).all(|(a, b)| a == b)
    }

    /// Strict ancestor test.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &VirtualPath) -> bool {
        self.segments.len() < other.segments.len() && self.is_ancestor_or_equal(other)
    }

    /// Canonical rendering.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtualPath({:?})", self.text)
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn p(s: &str) -> VirtualPath {
        VirtualPath::parse(s).expect("valid path")
    }

    #[test]
    fn accepts_canonical_absolute_paths() {
        assert_eq!(p("/").depth(), 0);
        assert_eq!(p("/a").depth(), 1);
        assert_eq!(p("/a/b/c").segments().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(p("/a.json").file_name(), Some("a.json"));
    }

    #[test]
    fn rejects_relative_traversal_and_malformed() {
        for bad in [
            "", "a/b", "/a/../b", "/..", "/./a", "/a//b", "/a/", "/a\\b", "/a\0b",
        ] {
            let e = VirtualPath::parse(bad).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::InvalidPath, "for {bad:?}");
        }
        let long = format!("/{}", "x".repeat(MAX_SEGMENT_LEN + 1));
        assert_eq!(
            VirtualPath::parse(&long).unwrap_err().kind(),
            ErrorKind::InvalidPath
        );
    }

    #[test]
    fn subtree_check_is_segment_wise_not_string_prefix() {
        assert!(p("/a/b").is_ancestor_of(&p("/a/b/c")));
        assert!(p("/a/b").is_ancestor_or_equal(&p("/a/b")));
        assert!(!p("/a/b").is_ancestor_or_equal(&p("/a/bc")));
        assert!(!p("/a/bc").is_ancestor_or_equal(&p("/a/b")));
        assert!(p("/").is_ancestor_of(&p("/anything")));
    }

    #[test]
    fn parent_and_join_roundtrip() {
        let deep = p("/a/b/c");
        assert_eq!(deep.parent(), Some(p("/a/b")));
        assert_eq!(p("/a/b").join_segment("c").unwrap(), deep);
        assert_eq!(p("/").join_segment("a").unwrap(), p("/a"));
        assert_eq!(p("/a").parent(), Some(VirtualPath::root()));
        assert_eq!(VirtualPath::root().parent(), None);
        for bad in ["..", ".", "", "a/b", "a\\b", "x\0"] {
            assert!(p("/a").join_segment(bad).is_err(), "for {bad:?}");
        }
    }
}
