//! Tagged value model: **inline bytes** or **external reference**.
//!
//! Metadata workloads typically have a bimodal value distribution:
//!
//! - **Small payloads** (≤ a few KB) — directory entries, xattrs,
//!   small JSON descriptors — get inlined directly into the
//!   metadata blob for sub-microsecond reads with no extra hop.
//! - **Large payloads** (images, blobs, datasets) — live in a
//!   separate object store and are referenced by URL
//!   (`s3://bucket/key`, `https://cdn.example/...`,
//!   `ipfs://...`).
//!
//! [`Value`] makes this split a first-class API choice via two
//! tagged variants. Encoding overhead is a single byte — the engine
//! stores `[tag | body]` and the tag tells the reader how to
//! interpret the body.
//!
//! ```ignore
//! use artisan::{Tree, Value};
//!
//! let tree = Tree::open_in_memory()?;
//! tree.put_inline(b"meta/01", b"hello world")?;
//! tree.put_ref(b"img/big.png", "s3://photos/big.png")?;
//!
//! match tree.get_value(b"meta/01")? {
//!     Some(Value::Inline(bytes)) => /* serve inline */,
//!     Some(Value::External(url)) => /* fetch from object store */,
//!     None => /* not found */,
//! }
//! ```

use super::errors::{Error, Result};

/// Tag byte preceding an inline payload. Stored as the first byte
/// of every encoded inline value.
pub const TAG_INLINE: u8 = 0x00;

/// Tag byte preceding an external-reference payload (UTF-8 string).
pub const TAG_EXTERNAL: u8 = 0x01;

/// A metadata value — either inlined bytes or an external reference.
///
/// See the module docs for the rationale and encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Raw bytes stored directly in the metadata blob.
    ///
    /// Recommended for payloads ≤ a few KB. Hard ceiling is
    /// `u16::MAX - 1` (= 65 534 bytes) since the engine's leaf
    /// `value_size` is a `u16` and the tag byte takes one of those.
    Inline(Vec<u8>),

    /// Opaque UTF-8 reference to data living outside artisan —
    /// typically a URL such as `s3://bucket/key`,
    /// `https://cdn.example/path`, or `ipfs://...`. The engine does
    /// not parse or validate the string; callers resolve it.
    External(String),
}

impl Value {
    /// Convenience constructor: inline payload from anything that
    /// can convert to `Vec<u8>`.
    #[must_use]
    pub fn inline<B: Into<Vec<u8>>>(bytes: B) -> Self {
        Self::Inline(bytes.into())
    }

    /// Convenience constructor: external reference from anything
    /// that can convert to `String`.
    #[must_use]
    pub fn external<S: Into<String>>(url: S) -> Self {
        Self::External(url.into())
    }

    /// Borrow the payload if this is an inline value.
    #[must_use]
    pub fn as_inline(&self) -> Option<&[u8]> {
        match self {
            Self::Inline(b) => Some(b),
            Self::External(_) => None,
        }
    }

    /// Borrow the URL if this is an external reference.
    #[must_use]
    pub fn as_external(&self) -> Option<&str> {
        match self {
            Self::External(s) => Some(s),
            Self::Inline(_) => None,
        }
    }

    /// `true` iff this is [`Value::Inline`].
    #[must_use]
    pub fn is_inline(&self) -> bool {
        matches!(self, Self::Inline(_))
    }

    /// `true` iff this is [`Value::External`].
    #[must_use]
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Number of bytes the encoded form will occupy in the leaf
    /// extent (1 tag byte + body length).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        1 + match self {
            Self::Inline(b) => b.len(),
            Self::External(s) => s.len(),
        }
    }

    /// Encode to the on-disk byte form `[tag | body]`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        match self {
            Self::Inline(b) => {
                out.push(TAG_INLINE);
                out.extend_from_slice(b);
            }
            Self::External(s) => {
                out.push(TAG_EXTERNAL);
                out.extend_from_slice(s.as_bytes());
            }
        }
        out
    }

    /// Decode from on-disk byte form.
    ///
    /// Errors when `bytes` is empty, when the leading tag is
    /// unrecognised, or when an `External` body is not valid UTF-8.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (tag, body) = bytes.split_first().ok_or(Error::InvalidValueEncoding {
            context: "value blob is empty",
        })?;
        match *tag {
            TAG_INLINE => Ok(Self::Inline(body.to_vec())),
            TAG_EXTERNAL => {
                let s = std::str::from_utf8(body).map_err(|_| Error::InvalidValueEncoding {
                    context: "External tag with non-UTF-8 body",
                })?;
                Ok(Self::External(s.to_owned()))
            }
            _ => Err(Error::InvalidValueEncoding {
                context: "unknown tag byte",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_round_trip() {
        let v = Value::inline(b"hello world".to_vec());
        let encoded = v.encode();
        assert_eq!(encoded[0], TAG_INLINE);
        assert_eq!(&encoded[1..], b"hello world");
        let back = Value::decode(&encoded).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn external_round_trip() {
        let v = Value::external("s3://bucket/img/01.jpg");
        let encoded = v.encode();
        assert_eq!(encoded[0], TAG_EXTERNAL);
        assert_eq!(&encoded[1..], b"s3://bucket/img/01.jpg");
        let back = Value::decode(&encoded).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn empty_inline_round_trips() {
        let v = Value::Inline(Vec::new());
        let encoded = v.encode();
        assert_eq!(encoded, vec![TAG_INLINE]);
        let back = Value::decode(&encoded).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn empty_external_round_trips() {
        let v = Value::External(String::new());
        let encoded = v.encode();
        assert_eq!(encoded, vec![TAG_EXTERNAL]);
        let back = Value::decode(&encoded).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn decode_empty_blob_errors() {
        let r = Value::decode(&[]);
        assert!(matches!(r, Err(Error::InvalidValueEncoding { .. })));
    }

    #[test]
    fn decode_unknown_tag_errors() {
        let r = Value::decode(&[0x99, 0xAB, 0xCD]);
        assert!(matches!(r, Err(Error::InvalidValueEncoding { .. })));
    }

    #[test]
    fn decode_external_with_non_utf8_errors() {
        let bytes = vec![TAG_EXTERNAL, 0xFF, 0xFE, 0xFD];
        let r = Value::decode(&bytes);
        assert!(matches!(r, Err(Error::InvalidValueEncoding { .. })));
    }

    #[test]
    fn accessors() {
        let i = Value::inline(b"hi".to_vec());
        assert!(i.is_inline());
        assert_eq!(i.as_inline(), Some(&b"hi"[..]));
        assert_eq!(i.as_external(), None);

        let e = Value::external("ipfs://Qm...");
        assert!(e.is_external());
        assert_eq!(e.as_external(), Some("ipfs://Qm..."));
        assert_eq!(e.as_inline(), None);
    }

    #[test]
    fn encoded_len_matches_encode() {
        let v = Value::inline(vec![0u8; 1000]);
        assert_eq!(v.encoded_len(), v.encode().len());
        let v2 = Value::external("https://example.org/file");
        assert_eq!(v2.encoded_len(), v2.encode().len());
    }
}
