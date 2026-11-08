//! Images the user attaches to a message.
//!
//! An [`Image`] is only ever built from its bytes: the format is read from the
//! magic number, never from a file name or a caller's claim, so a `.png` that
//! is really a script is refused, and a screenshot saved without an extension
//! is not. The size cap is checked at the same moment, because an image that
//! exists at all is one every request of the session will carry.
//!
//! # Why the size counted for the history is nominal
//!
//! [`Image::NOMINAL_BYTES`] is what an image weighs in
//! [`Message::approx_bytes`](super::message::Message::approx_bytes), not its
//! base64 length. The history budget measures text the model reads, and a
//! provider charges an image by its pixels, not by its encoding - a 2 MiB
//! screenshot is a couple of thousand tokens, while its 2.7 MB of base64 would
//! fill the 256 KB budget ten times over and push every other message out.
//!
//! # The session log
//!
//! The record keeps the image itself, base64 encoded, so a resumed session
//! sends the model what it saw the first time. The per-image cap is what keeps
//! that record bounded.

use std::fmt;

use serde::de::Error as _;
use serde::ser::SerializeStruct as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ImageError;

/// The formats every provider that accepts images at all accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageMediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageMediaType {
    /// The format `bytes` start with, if it is one of these.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    /// The MIME type, as both vendor APIs spell it.
    pub fn as_mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The short name a person recognises: `png`, `jpeg`, ...
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    fn from_mime(mime: &str) -> Option<Self> {
        [Self::Png, Self::Jpeg, Self::Gif, Self::Webp]
            .into_iter()
            .find(|kind| kind.as_mime() == mime)
    }
}

/// An image, validated: a known format by its magic number, and no larger than
/// [`Image::MAX_BYTES`].
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    media_type: ImageMediaType,
    data: Vec<u8>,
}

impl Image {
    /// The largest image accepted: 3,750,000 bytes, whose base64 form is
    /// 5,000,000 bytes - under Anthropic's limit for one image (measured on
    /// the encoded data) whether "5 MB" means 5,000,000 or 5 MiB. One cap
    /// for every provider, so an image that attaches is one that any of them
    /// can be sent.
    pub const MAX_BYTES: usize = 3_750_000;

    /// What one image counts for in the history's size measure. See the
    /// module docs for why it is not the encoded length.
    pub const NOMINAL_BYTES: usize = 1536;

    pub fn from_bytes(data: Vec<u8>) -> Result<Self, ImageError> {
        if data.len() > Self::MAX_BYTES {
            return Err(ImageError::TooLarge {
                actual: data.len(),
                limit: Self::MAX_BYTES,
            });
        }
        let media_type = ImageMediaType::sniff(&data).ok_or(ImageError::UnknownFormat)?;
        Ok(Self { media_type, data })
    }

    pub fn media_type(&self) -> ImageMediaType {
        self.media_type
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Never true: no supported format has an empty encoding. Present because
    /// a `len` without it is a clippy error.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The bytes in standard, padded base64 - what both vendor APIs and the
    /// session log carry.
    pub fn to_base64(&self) -> String {
        base64::encode(&self.data)
    }
}

/// `[image: 12 KB png]` - how an image appears wherever only text can: in a
/// terminal, in a compaction transcript, and to a model that cannot see it.
impl fmt::Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[image: {} KB {}]",
            self.data.len().div_ceil(1024),
            self.media_type.short_name()
        )
    }
}

/// Without the bytes: a multi-megabyte debug dump helps nobody.
impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Written by hand rather than through a `#[serde(into = ...)]` proxy, which
/// would copy up to [`Image::MAX_BYTES`] on every append to the session log.
impl Serialize for Image {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Image", 2)?;
        state.serialize_field("media_type", self.media_type.as_mime())?;
        state.serialize_field("data", &self.to_base64())?;
        state.end()
    }
}

/// Validated like [`Image::from_bytes`]: a log edited by hand, or written by a
/// build with a larger cap, cannot smuggle in what this one would refuse. The
/// stored media type has to agree with the bytes.
impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            media_type: String,
            data: String,
        }

        let stored = Stored::deserialize(deserializer)?;
        let claimed = ImageMediaType::from_mime(&stored.media_type).ok_or_else(|| {
            D::Error::custom(format!("unsupported image type `{}`", stored.media_type))
        })?;
        let bytes = base64::decode(&stored.data)
            .ok_or_else(|| D::Error::custom("image data is not valid base64"))?;
        let image = Image::from_bytes(bytes).map_err(D::Error::custom)?;
        if image.media_type != claimed {
            return Err(D::Error::custom(format!(
                "image recorded as {} but its bytes are {}",
                claimed.as_mime(),
                image.media_type.as_mime()
            )));
        }
        Ok(image)
    }
}

/// Standard base64 (RFC 4648 §4, padded). Forty lines here instead of a
/// dependency in the crate everything else depends on.
mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub(super) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[((n >> shift) & 0x3F) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Each byte's 6-bit value, or `INVALID`.
    const INVALID: u8 = 0xFF;
    const DECODE: [u8; 256] = {
        let mut table = [INVALID; 256];
        let mut index = 0;
        while index < 64 {
            table[ALPHABET[index] as usize] = index as u8;
            index += 1;
        }
        table
    };

    /// `None` for anything but padded base64 in the standard alphabet.
    pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.as_bytes();
        if text.len() % 4 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(text.len() / 4 * 3);
        let quads = text.len() / 4;
        for (index, quad) in text.chunks(4).enumerate() {
            let padding = quad.iter().rev().take_while(|&&c| c == b'=').count();
            if padding > 2 || (padding > 0 && index + 1 != quads) {
                return None;
            }
            let mut n = 0u32;
            for &c in &quad[..4 - padding] {
                let value = DECODE[usize::from(c)];
                if value == INVALID {
                    return None;
                }
                n = (n << 6) | u32::from(value);
            }
            n <<= 6 * padding as u32;
            let decoded = n.to_be_bytes();
            out.extend_from_slice(&decoded[1..4 - padding]);
        }
        Some(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The smallest valid PNG: one transparent pixel.
    pub(crate) const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn the_format_comes_from_the_magic_number() {
        assert_eq!(ImageMediaType::sniff(TINY_PNG), Some(ImageMediaType::Png));
        assert_eq!(
            ImageMediaType::sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some(ImageMediaType::Jpeg)
        );
        assert_eq!(
            ImageMediaType::sniff(b"GIF89a\x01\x00"),
            Some(ImageMediaType::Gif)
        );
        assert_eq!(
            ImageMediaType::sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageMediaType::Webp)
        );
        assert_eq!(ImageMediaType::sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(ImageMediaType::sniff(b"#!/bin/sh\necho hi"), None);
        assert_eq!(ImageMediaType::sniff(b""), None);
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_whatever_they_are_called() {
        assert_eq!(
            Image::from_bytes(b"not a png at all".to_vec()),
            Err(ImageError::UnknownFormat)
        );
    }

    #[test]
    fn an_image_over_the_cap_is_refused_at_construction() {
        let mut big = TINY_PNG.to_vec();
        big.resize(Image::MAX_BYTES + 1, 0);
        assert_eq!(
            Image::from_bytes(big),
            Err(ImageError::TooLarge {
                actual: Image::MAX_BYTES + 1,
                limit: Image::MAX_BYTES
            })
        );

        let mut at_cap = TINY_PNG.to_vec();
        at_cap.resize(Image::MAX_BYTES, 0);
        let at_cap = Image::from_bytes(at_cap).expect("the cap itself is allowed");
        assert!(
            at_cap.to_base64().len() <= 5 * 1024 * 1024,
            "the largest image must stay within Anthropic's 5 MiB of base64: {}",
            at_cap.to_base64().len()
        );
    }

    #[test]
    fn it_displays_as_its_size_and_format() {
        let image = Image::from_bytes(TINY_PNG.to_vec()).unwrap();
        assert_eq!(image.to_string(), "[image: 1 KB png]");

        let mut bigger = TINY_PNG.to_vec();
        bigger.resize(10 * 1024 + 1, 0);
        assert_eq!(
            Image::from_bytes(bigger).unwrap().to_string(),
            "[image: 11 KB png]"
        );
    }

    #[test]
    fn base64_matches_the_rfc_test_vectors_and_round_trips() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64::encode(raw.as_bytes()), encoded);
            assert_eq!(base64::decode(encoded).unwrap(), raw.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64::decode(&base64::encode(&all)).unwrap(), all);

        for broken in ["Zg=", "Z===", "Zg==Zm9v", "Zm9*", "Zm9v\n"] {
            assert_eq!(base64::decode(broken), None, "{broken:?}");
        }
    }

    #[test]
    fn a_stored_image_must_agree_with_its_bytes() {
        let image = Image::from_bytes(TINY_PNG.to_vec()).unwrap();
        let json = serde_json::to_value(&image).unwrap();
        assert_eq!(json["media_type"], "image/png");
        assert_eq!(
            serde_json::from_value::<Image>(json.clone()).unwrap(),
            image
        );

        let mut lying = json.clone();
        lying["media_type"] = "image/gif".into();
        assert!(serde_json::from_value::<Image>(lying).is_err());

        let mut garbage = json;
        garbage["data"] = "@@@@".into();
        assert!(serde_json::from_value::<Image>(garbage).is_err());
    }
}
