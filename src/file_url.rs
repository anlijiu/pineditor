//! `file://` URIs for paths on disk.
//!
//! `definition` has to name the file a symbol was declared in. gpui-base treats
//! only `http(s)` targets as external — everything else jumps inside the current
//! document — and `show_document` is the hook that takes over. That hook receives
//! a URI, so a library file has to survive the round trip: encode here, decode
//! there.
//!
//! `lsp-types`' `Uri` wraps `fluent_uri`, which has no `to_file_path`; the
//! percent-encoding rules are RFC 3986's unreserved set plus `/`, so this is all
//! that is needed.

use std::path::{Path, PathBuf};

use lsp_types::Uri;

/// Whether a byte can appear in a URI path as-is.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// A `file://` URI naming `path`.
pub fn encode(path: &Path) -> Uri {
    let mut encoded = String::with_capacity(path.as_os_str().len() + 8);
    for byte in path.to_string_lossy().as_bytes() {
        if is_unreserved(*byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    // An absolute path starts with `/`, so `file://` + path is `file:///…`.
    let uri = format!("file://{encoded}");
    uri.parse()
        .unwrap_or_else(|_| "file:///pine".parse().expect("a valid fallback URI"))
}

/// The path a `file://` URI names, or `None` for any other scheme.
pub fn decode(uri: &Uri) -> Option<PathBuf> {
    if uri.scheme().map(|scheme| scheme.as_str()) != Some("file") {
        return None;
    }

    let path = uri.path().as_str();
    let mut bytes = Vec::with_capacity(path.len());
    let mut chars = path.bytes();
    while let Some(byte) = chars.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = chars.next()?;
        let low = chars.next()?;
        let pair = [high, low];
        let hex = std::str::from_utf8(&pair).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
    }

    let path = String::from_utf8(bytes).ok()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(path: &str) -> PathBuf {
        decode(&encode(Path::new(path))).expect("decodes")
    }

    #[test]
    fn a_plain_path_round_trips() {
        assert_eq!(round_trip("/tmp/project/mylib.pine"), PathBuf::from("/tmp/project/mylib.pine"));
    }

    #[test]
    fn spaces_and_non_ascii_round_trip() {
        // Both need percent-encoding, and the bytes — not chars — must survive.
        assert_eq!(
            round_trip("/tmp/my project/策略.pine"),
            PathBuf::from("/tmp/my project/策略.pine")
        );
    }

    #[test]
    fn the_uri_keeps_its_file_scheme() {
        let uri = encode(Path::new("/tmp/a.pine"));
        assert_eq!(uri.scheme().map(|scheme| scheme.as_str()), Some("file"));
        assert_eq!(uri.path().as_str(), "/tmp/a.pine");
    }

    #[test]
    fn another_scheme_does_not_decode() {
        let http: Uri = "https://example.com/x".parse().expect("valid uri");
        assert!(decode(&http).is_none());
    }
}
