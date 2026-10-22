//! The one place gpui decodes raster images, so that every caller gets the same bounds, the
//! same EXIF orientation and the same tolerance for icon files.

use image::{
    DynamicImage, ExtendedColorType, ImageDecoder as _, ImageFormat, ImageReader, ImageResult,
    Limits, metadata::Orientation,
};
use std::io::Cursor;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// What an image file says about itself, read without decoding any pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageHeader {
    /// The width in pixels as the image is shown, after its EXIF orientation is applied.
    pub width: u32,
    /// The height in pixels as the image is shown, after its EXIF orientation is applied.
    pub height: u32,
    /// The pixel layout the decoder produces.
    pub color_type: ExtendedColorType,
}

/// Reads the header of `bytes` as `format`. An unreadable orientation tag is treated as "upright".
pub fn image_header(bytes: &[u8], format: ImageFormat) -> ImageResult<ImageHeader> {
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = if swaps_axes(orientation) {
        (height, width)
    } else {
        (width, height)
    };
    Ok(ImageHeader {
        width,
        height,
        color_type: decoder.color_type().into(),
    })
}

/// Decodes `bytes` as `format` within `limits`, upright according to its EXIF orientation.
///
/// An ICO file whose largest entry is a PNG that is not RGBA, which the `image` crate refuses,
/// is decoded from that entry.
pub fn decode_image(
    bytes: &[u8],
    format: ImageFormat,
    limits: Limits,
) -> ImageResult<DynamicImage> {
    match decode_oriented(bytes, format, limits.clone()) {
        Err(error) if format == ImageFormat::Ico => match largest_png_entry(bytes) {
            Some(png) => decode_oriented(png, ImageFormat::Png, limits)
                .map(|image| DynamicImage::ImageRgba8(image.into_rgba8())),
            None => Err(error),
        },
        result => result,
    }
}

fn decode_oriented(bytes: &[u8], format: ImageFormat, limits: Limits) -> ImageResult<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    // The same accounting `ImageReader::decode` does: the pixel buffer is charged to the budget
    // before it is allocated.
    let mut limits = limits;
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;

    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// The PNG payload of the entry an ICO viewer would show: the one with the greatest bit depth,
/// and among those the greatest area. `None` when that entry is not a PNG.
fn largest_png_entry(ico: &[u8]) -> Option<&[u8]> {
    const HEADER_LEN: usize = 6;
    const ENTRY_LEN: usize = 16;
    if ico.get(..4)? != [0, 0, 1, 0] {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]));
    let best = (0..count)
        .filter_map(|ix| {
            let start = HEADER_LEN + ix * ENTRY_LEN;
            let entry = ico.get(start..start + ENTRY_LEN)?;
            // A stored size of 0 means 256.
            let side = |stored: u8| if stored == 0 { 256 } else { u32::from(stored) };
            let area = side(entry[0]) * side(entry[1]);
            let bit_count = u16::from_le_bytes([entry[6], entry[7]]);
            let len = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
            let payload = ico.get(offset..offset.checked_add(len)?)?;
            Some(((bit_count, area), payload))
        })
        .max_by_key(|(score, _)| *score)?;
    best.1.starts_with(PNG_SIGNATURE).then_some(best.1)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use image::{ExtendedColorType, ImageEncoder as _, Rgb, RgbImage, codecs::png::PngEncoder};

    pub(crate) fn png(image: &RgbImage, exif: Option<Vec<u8>>) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = PngEncoder::new(&mut bytes);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif).unwrap();
        }
        encoder
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        bytes
    }

    /// A little-endian TIFF block whose only tag is the orientation.
    pub(crate) fn exif_orientation(orientation: u16) -> Vec<u8> {
        let mut exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        exif.extend(1u16.to_le_bytes());
        exif.extend(0x0112u16.to_le_bytes());
        exif.extend(3u16.to_le_bytes());
        exif.extend(1u32.to_le_bytes());
        exif.extend(orientation.to_le_bytes());
        exif.extend([0, 0]);
        exif.extend(0u32.to_le_bytes());
        exif
    }

    /// A 4 x 2 picture with a red pixel at its top-left corner and blue everywhere else.
    pub(crate) fn landscape() -> RgbImage {
        let mut image = RgbImage::from_pixel(4, 2, Rgb([0, 0, 255]));
        image.put_pixel(0, 0, Rgb([255, 0, 0]));
        image
    }

    pub(crate) fn ico_with(entries: &[(u8, u8, u16, &[u8])]) -> Vec<u8> {
        let mut ico = vec![0, 0, 1, 0];
        ico.extend((entries.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * entries.len();
        for (width, height, bit_count, payload) in entries {
            ico.extend([*width, *height, 0, 0]);
            ico.extend(1u16.to_le_bytes());
            ico.extend(bit_count.to_le_bytes());
            ico.extend((payload.len() as u32).to_le_bytes());
            ico.extend((offset as u32).to_le_bytes());
            offset += payload.len();
        }
        for (_, _, _, payload) in entries {
            ico.extend(*payload);
        }
        ico
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use image::{ImageEncoder as _, Rgb, RgbImage, codecs::jpeg::JpegEncoder};

    #[test]
    fn an_ico_whose_png_entry_is_rgb_decodes() {
        let entry = png(&RgbImage::from_pixel(32, 32, Rgb([10, 20, 30])), None);
        let ico = ico_with(&[(32, 32, 24, &entry)]);

        assert!(
            image::load_from_memory_with_format(&ico, ImageFormat::Ico).is_err(),
            "the image crate's strict ICO decoder only takes RGBA PNG entries"
        );
        let decoded = decode_image(&ico, ImageFormat::Ico, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 32));
        assert_eq!(decoded.to_rgba8().get_pixel(5, 5).0, [10, 20, 30, 255]);

        let header = image_header(&ico, ImageFormat::Ico).unwrap();
        assert_eq!((header.width, header.height), (32, 32));
    }

    #[test]
    fn an_ico_decodes_from_its_largest_png_entry() {
        let small = png(&RgbImage::from_pixel(16, 16, Rgb([1, 1, 1])), None);
        let large = png(&RgbImage::from_pixel(48, 48, Rgb([9, 9, 9])), None);
        let ico = ico_with(&[(16, 16, 24, &small), (48, 48, 24, &large)]);
        let decoded = decode_image(&ico, ImageFormat::Ico, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (48, 48));
    }

    #[test]
    fn an_ico_with_an_entry_past_the_end_of_the_file_is_an_error() {
        let mut ico = vec![0, 0, 1, 0, 1, 0];
        ico.extend([16, 16, 0, 0, 1, 0, 32, 0]);
        ico.extend(1000u32.to_le_bytes());
        ico.extend(22u32.to_le_bytes());
        assert!(decode_image(&ico, ImageFormat::Ico, Limits::default()).is_err());
    }

    #[test]
    fn an_exif_orientation_of_6_is_drawn_upright() {
        let bytes = png(&landscape(), Some(exif_orientation(6)));
        let decoded = decode_image(&bytes, ImageFormat::Png, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 4));
        // The stored top-left corner of an orientation-6 picture is its top-right once upright.
        assert_eq!(decoded.to_rgb8().get_pixel(1, 0).0, [255, 0, 0]);
        assert_eq!(decoded.to_rgb8().get_pixel(0, 0).0, [0, 0, 255]);

        let header = image_header(&bytes, ImageFormat::Png).unwrap();
        assert_eq!((header.width, header.height), (2, 4));
    }

    #[test]
    fn every_orientation_places_the_corner_where_exif_says() {
        // For each orientation 1-8: where the stored top-left pixel ends up in a 4 x 2 picture.
        let expected = [
            (1, (0, 0), (4, 2)),
            (2, (3, 0), (4, 2)),
            (3, (3, 1), (4, 2)),
            (4, (0, 1), (4, 2)),
            (5, (0, 0), (2, 4)),
            (6, (1, 0), (2, 4)),
            (7, (1, 3), (2, 4)),
            (8, (0, 3), (2, 4)),
        ];
        for (orientation, corner, size) in expected {
            let bytes = png(&landscape(), Some(exif_orientation(orientation)));
            let decoded = decode_image(&bytes, ImageFormat::Png, Limits::default())
                .unwrap()
                .to_rgb8();
            assert_eq!(
                (decoded.width(), decoded.height()),
                size,
                "orientation {orientation}"
            );
            assert_eq!(
                decoded.get_pixel(corner.0, corner.1).0,
                [255, 0, 0],
                "orientation {orientation}"
            );
        }
    }

    #[test]
    fn an_unreadable_orientation_leaves_the_picture_as_stored() {
        let bytes = png(&landscape(), Some(exif_orientation(9)));
        let decoded = decode_image(&bytes, ImageFormat::Png, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 2));
        assert_eq!(image_header(&bytes, ImageFormat::Png).unwrap().width, 4);
    }

    #[test]
    fn a_jpeg_with_exif_orientation_6_is_drawn_upright() {
        let image = landscape();
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new(&mut bytes);
        encoder.set_exif_metadata(exif_orientation(6)).unwrap();
        encoder
            .write_image(image.as_raw(), 4, 2, ExtendedColorType::Rgb8)
            .unwrap();

        let decoded = decode_image(&bytes, ImageFormat::Jpeg, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 4));
        let header = image_header(&bytes, ImageFormat::Jpeg).unwrap();
        assert_eq!((header.width, header.height), (2, 4));
    }

    #[test]
    fn limits_still_refuse_an_image_that_is_too_large() {
        let bytes = png(&RgbImage::from_pixel(64, 64, Rgb([1, 2, 3])), None);
        let mut limits = Limits::no_limits();
        limits.max_alloc = Some(1024);
        assert!(decode_image(&bytes, ImageFormat::Png, limits).is_err());

        let mut narrow = Limits::no_limits();
        narrow.max_image_width = Some(32);
        assert!(decode_image(&bytes, ImageFormat::Png, narrow).is_err());
    }

    #[test]
    fn the_header_is_read_without_decoding_pixels() {
        // A PNG that stops after its header declares 16000 x 16000 pixels; reading the header must
        // not try to allocate or decode them.
        let mut header_only = PNG_SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(16000u32.to_be_bytes());
        ihdr.extend(16000u32.to_be_bytes());
        ihdr.extend([8, 0, 0, 0, 0]);
        header_only.extend(13u32.to_be_bytes());
        header_only.extend(b"IHDR");
        header_only.extend(&ihdr);
        header_only.extend(png_chunk_crc(b"IHDR", &ihdr).to_be_bytes());
        // The first pixel chunk, with no pixels in it.
        header_only.extend(0u32.to_be_bytes());
        header_only.extend(b"IDAT");
        header_only.extend(png_chunk_crc(b"IDAT", &[]).to_be_bytes());

        let header = image_header(&header_only, ImageFormat::Png).unwrap();
        assert_eq!((header.width, header.height), (16000, 16000));
        assert_eq!(header.color_type, ExtendedColorType::L8);
    }

    /// CRC-32 of a PNG chunk (type and data), for building a header-only fixture.
    fn png_chunk_crc(kind: &[u8], data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in kind.iter().chain(data) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}
