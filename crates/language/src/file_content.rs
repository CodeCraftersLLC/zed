pub const FILE_ANALYSIS_BYTES: usize = 1024;

#[derive(Debug, PartialEq)]
pub enum ByteContent {
    Utf16Le,
    Utf16Be,
    Binary,
    Unknown,
}

// Heuristic check using null byte distribution plus a generic text-likeness
// heuristic. This prefers UTF-16 when many bytes are NUL and otherwise
// distinguishes between text-like and binary-like content.
pub fn analyze_byte_content(bytes: &[u8]) -> ByteContent {
    if bytes.len() < 2 {
        return ByteContent::Unknown;
    }

    if is_known_binary_header(bytes) {
        return ByteContent::Binary;
    }

    let limit = bytes.len().min(FILE_ANALYSIS_BYTES);
    let mut even_null_count = 0usize;
    let mut odd_null_count = 0usize;
    let mut non_text_like_count = 0usize;

    for (i, &byte) in bytes[..limit].iter().enumerate() {
        if byte == 0 {
            if i % 2 == 0 {
                even_null_count += 1;
            } else {
                odd_null_count += 1;
            }
            non_text_like_count += 1;
            continue;
        }

        let is_text_like = match byte {
            b'\t' | b'\n' | b'\r' | 0x0C => true,
            0x20..=0x7E => true,
            // Treat bytes that are likely part of UTF-8 or single-byte encodings as text-like.
            0x80..=0xBF | 0xC2..=0xF4 => true,
            _ => false,
        };

        if !is_text_like {
            non_text_like_count += 1;
        }
    }

    let total_null_count = even_null_count + odd_null_count;

    // If there are no NUL bytes at all, this is overwhelmingly likely to be text.
    if total_null_count == 0 {
        return ByteContent::Unknown;
    }

    // A lone NUL in a short file is stray data, not the high bytes of UTF-16 text: with integer
    // division `limit / 16` is 0 or 1 for any sample under 32 bytes, so one NUL used to qualify.
    // Past two, the threshold stays the rounded-down one: text that is mostly non-Latin has few NULs.
    let has_significant_nulls = total_null_count >= 2 && total_null_count >= limit / 16;
    let nulls_skew_to_even = even_null_count > odd_null_count * 4;
    let nulls_skew_to_odd = odd_null_count > even_null_count * 4;

    if has_significant_nulls {
        let sample = &bytes[..limit];

        // UTF-16BE ASCII: [0x00, char] — nulls at even positions (high byte first)
        // UTF-16LE ASCII: [char, 0x00] — nulls at odd positions (low byte first)

        if nulls_skew_to_even && is_plausible_utf16_text(sample, false) {
            return ByteContent::Utf16Be;
        }

        if nulls_skew_to_odd && is_plausible_utf16_text(sample, true) {
            return ByteContent::Utf16Le;
        }

        return ByteContent::Binary;
    }

    if non_text_like_count * 100 < limit * 8 {
        ByteContent::Unknown
    } else {
        ByteContent::Binary
    }
}

/// A leading signature that real text almost never starts with. Short ASCII tags (`ID3`, `RIFF`,
/// `OggS`, `fLaC`) are only trusted with the bytes the format puts right after them, so "RIFF
/// format notes" stays text. `cherrypick-git`'s `text_decode` classifies the same signatures for
/// the preview and copy side: keep the two tables identical.
fn is_known_binary_header(bytes: &[u8]) -> bool {
    const SIGNATURES: &[&[u8]] = &[
        b"%PDF-",
        b"PK\x03\x04", // ZIP local header
        b"PK\x05\x06", // ZIP end of central directory
        b"PK\x07\x08", // ZIP spanning/splitting
        b"\x89PNG\r\n\x1a\n",
        b"\xFF\xD8\xFF", // JPEG
        b"GIF87a",
        b"GIF89a",
        b"\xFF\xFB", // MP3 frame sync (MPEG1 Layer3)
        b"\xFF\xFA", // MP3 frame sync (MPEG1 Layer3)
        b"\xFF\xF3", // MP3 frame sync (MPEG2 Layer3)
        b"\xFF\xF2", // MP3 frame sync (MPEG2 Layer3)
    ];
    if SIGNATURES
        .iter()
        .any(|signature| bytes.starts_with(signature))
    {
        return true;
    }
    let after = |tag: &[u8]| bytes.strip_prefix(tag);
    // RIFF <size> <form>: WAV, AVI, WebP.
    let riff = after(b"RIFF")
        .is_some_and(|rest| matches!(rest.get(4..8), Some(b"WAVE" | b"AVI " | b"WEBP")));
    // ID3v2 <major 2-4> <revision>.
    let id3 = after(b"ID3").is_some_and(|rest| matches!(rest.first(), Some(2..=4)));
    // Ogg page: version byte 0.
    let ogg = after(b"OggS").is_some_and(|rest| rest.first() == Some(&0));
    // FLAC stream marker is followed by a STREAMINFO block header (type 0..=6).
    let flac = after(b"fLaC").is_some_and(|rest| rest.first().is_some_and(|b| b & 0x7F <= 6));
    riff || id3 || ogg || flac
}

// Null byte skew alone is not enough to identify UTF-16 -- binary formats with
// small 16-bit values (like PCM audio) produce the same pattern. Decode the
// bytes as UTF-16 and reject if too many code units land in control character
// ranges or form unpaired surrogates, which real text almost never contains.
fn is_plausible_utf16_text(bytes: &[u8], little_endian: bool) -> bool {
    let mut suspicious_count = 0usize;
    let mut total = 0usize;

    let mut i = 0;
    while let Some(code_unit) = read_u16(bytes, i, little_endian) {
        total += 1;

        match code_unit {
            0x0009 | 0x000A | 0x000C | 0x000D => {}
            // C0/C1 control characters and non-characters
            0x0000..=0x001F | 0x007F..=0x009F | 0xFFFE | 0xFFFF => suspicious_count += 1,
            0xD800..=0xDBFF => {
                let next_offset = i + 2;
                let has_low_surrogate = read_u16(bytes, next_offset, little_endian)
                    .is_some_and(|next| (0xDC00..=0xDFFF).contains(&next));
                if has_low_surrogate {
                    total += 1;
                    i += 2;
                } else {
                    suspicious_count += 1;
                }
            }
            // Lone low surrogate without a preceding high surrogate
            0xDC00..=0xDFFF => suspicious_count += 1,
            _ => {}
        }

        i += 2;
    }

    if total == 0 {
        return false;
    }

    // Real UTF-16 text has near-zero control characters; binary data with
    // small 16-bit values typically exceeds 5%. 2% provides a safe margin.
    suspicious_count * 100 < total * 2
}

fn read_u16(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u16> {
    let pair = [*bytes.get(offset)?, *bytes.get(offset + 1)?];
    if little_endian {
        return Some(u16::from_le_bytes(pair));
    }
    Some(u16::from_be_bytes(pair))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_nul_in_a_short_text_file_is_not_utf16() {
        assert_eq!(
            analyze_byte_content(b"text before\0text after"),
            ByteContent::Unknown
        );
        // Three bytes are too few to call text: the non-text ratio still marks them binary.
        assert_eq!(analyze_byte_content(b"a\0b"), ByteContent::Binary);

        let utf16le: Vec<u8> = "hi there"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(utf16le.iter().filter(|byte| **byte == 0).count(), 8);
        assert_eq!(analyze_byte_content(&utf16le), ByteContent::Utf16Le);
        let utf16be: Vec<u8> = "hi there"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(analyze_byte_content(&utf16be), ByteContent::Utf16Be);
    }

    #[test]
    fn utf16_text_with_few_ascii_characters_is_still_utf16() {
        // Two NULs in 40 bytes: "AB" followed by eighteen Cyrillic letters, which have none.
        let text = format!("AB{}", "\u{416}".repeat(18));
        let utf16le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(utf16le.len(), 40);
        assert_eq!(utf16le.iter().filter(|byte| **byte == 0).count(), 2);
        assert_eq!(analyze_byte_content(&utf16le), ByteContent::Utf16Le);
        let utf16be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(analyze_byte_content(&utf16be), ByteContent::Utf16Be);
    }

    #[test]
    fn ascii_text_that_starts_like_a_binary_tag_is_still_text() {
        for text in [
            "ID3 tags are described here",
            "RIFF format notes",
            "OggS is a page marker",
            "fLaC is a codec",
            "IWAD and PWAD name Doom archives",
            "PWAD files patch an IWAD",
        ] {
            assert_eq!(analyze_byte_content(text.as_bytes()), ByteContent::Unknown);
        }
    }

    #[test]
    fn a_tag_followed_by_what_its_format_puts_there_is_binary() {
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend_from_slice(&[0, 1, 2, 3]);
        let tagged = |tag: &[u8], then: &[u8]| [tag, then, &[b'a'; 64]].concat();
        for bytes in [
            wav,
            tagged(b"RIFF\x24\0\0\0", b"AVI LIST"),
            tagged(b"RIFF\x24\0\0\0", b"WEBPVP8 "),
            tagged(b"ID3", b"\x02\0"),
            tagged(b"ID3", b"\x04\0"),
            tagged(b"OggS", b"\0\x02"),
            tagged(b"fLaC", b"\0\0\0\x22"),
            tagged(b"fLaC", b"\x86\0\0\x22"),
        ] {
            assert_eq!(
                analyze_byte_content(&bytes),
                ByteContent::Binary,
                "{:?}",
                &bytes[..12]
            );
        }
    }

    #[test]
    fn a_tag_without_what_its_format_puts_after_it_is_text() {
        let tagged = |tag: &[u8], then: &[u8]| [tag, then, &[b'a'; 64]].concat();
        for bytes in [
            tagged(b"RIFF", b" is a container, "),
            tagged(b"RIFF\x24\0\0\0", b"JUNK"),
            tagged(b"ID3", b"\x01\0"),
            tagged(b"ID3", b"\x05\0"),
            tagged(b"OggS", b"\x01\0"),
            tagged(b"fLaC", b"\x07\0\0\x22"),
            b"RIFF".to_vec(),
            b"ID3".to_vec(),
        ] {
            assert_eq!(
                analyze_byte_content(&bytes),
                ByteContent::Unknown,
                "{:?}",
                &bytes[..bytes.len().min(12)]
            );
        }
    }

    #[test]
    fn a_few_nuls_among_many_bytes_are_not_utf16() {
        let mut text = vec![b'a'; FILE_ANALYSIS_BYTES];
        text[100] = 0;
        assert_eq!(analyze_byte_content(&text), ByteContent::Unknown);
    }
}
