//! What kind of picture some bytes are, and how big, read from their header.
//!
//! Read rather than asked for, because the two things a panel needs from a
//! picture (a name ending the client will draw, and a size to hold room for)
//! are both in the first few hundred bytes, and a caller who got them wrong
//! would get a picture that is not drawn, or a panel that jumps when it lands.
//!
//! Only the four kinds Rift draws in a panel (WIRE.md §5). Anything else is
//! `None`: an SVG is a document rather than a picture, and is exactly the
//! stranger's-code-in-every-client a panel exists to keep out.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    pub extension: &'static str,
    pub content_type: &'static str,
    /// `None` when the header did not say: the client then sizes it on arrival.
    pub size: Option<(u32, u32)>,
}

fn be16(b: &[u8], at: usize) -> u32 {
    u16::from_be_bytes([b[at], b[at + 1]]) as u32
}
fn le16(b: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([b[at], b[at + 1]]) as u32
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le24(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], 0])
}

pub fn image_info(b: &[u8]) -> Option<ImageInfo> {
    if b.len() >= 24 && b.starts_with(&[0x89, b'P', b'N', b'G']) && &b[12..16] == b"IHDR" {
        return Some(sized("png", "image/png", Some((be32(b, 16), be32(b, 20)))));
    }
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some(sized("gif", "image/gif", Some((le16(b, 6), le16(b, 8)))));
    }
    if b.len() >= 3 && b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(sized("jpg", "image/jpeg", jpeg_size(b)));
    }
    if b.len() >= 16 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some(sized("webp", "image/webp", webp_size(b)));
    }
    None
}

fn sized(
    extension: &'static str,
    content_type: &'static str,
    size: Option<(u32, u32)>,
) -> ImageInfo {
    let ok = |n: u32| (1..=16384).contains(&n);
    ImageInfo {
        extension,
        content_type,
        size: size.filter(|&(w, h)| ok(w) && ok(h)),
    }
}

/// The first start-of-frame marker's size. Every marker before it is skipped
/// by its length.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    while at + 9 < b.len() {
        if b[at] != 0xff {
            return None;
        }
        let marker = b[at + 1];
        // SOF0–SOF15, less the three that are not frames: DHT, JPG and DAC.
        if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
            return Some((be16(b, at + 7), be16(b, at + 5)));
        }
        at += 2 + be16(b, at + 2) as usize;
    }
    None
}

/// Lossy, lossless or extended: the three ways a WebP says how big it is.
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    match &b[12..16] {
        b"VP8 " if b.len() >= 30 => Some((le16(b, 26) & 0x3fff, le16(b, 28) & 0x3fff)),
        b"VP8L" if b.len() >= 25 => {
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" if b.len() >= 30 => Some((le24(b, 24) + 1, le24(b, 27) + 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// Every way each format can say how big it is, from real files ffmpeg
    /// made at 37×23.
    #[test]
    fn reads_the_kind_and_the_size_of_each_format_a_panel_draws() {
        for (file, extension) in [
            ("37x23.png", "png"),
            ("37x23.jpg", "jpg"),
            ("37x23.gif", "gif"),
            ("37x23.webp", "webp"),
            ("37x23-lossless.webp", "webp"),
            ("37x23-alpha.webp", "webp"),
        ] {
            let info = image_info(&fixture(file)).unwrap();
            assert_eq!(info.extension, extension, "{file}");
            assert_eq!(info.size, Some((37, 23)), "{file}");
        }
    }

    #[test]
    fn anything_else_is_not_a_picture_a_panel_can_show() {
        assert_eq!(
            image_info(br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#),
            None
        );
        assert_eq!(image_info(&[]), None);
    }

    #[test]
    fn a_header_cut_short_still_names_the_kind_without_a_size() {
        let info = image_info(&fixture("37x23.jpg")[..4]).unwrap();
        assert_eq!((info.extension, info.size), ("jpg", None));
    }
}
