//! The one place gpui decodes raster images, so that every caller gets the same bounds, the
//! same EXIF orientation and the same tolerance for icon files.

use crate::{MAX_RASTER_BYTES, MAX_RASTER_SIDE};
use image::{
    AnimationDecoder as _, ColorType, DynamicImage, ExtendedColorType, Frame, Frames,
    ImageDecoder as _, ImageError, ImageFormat, ImageReader, ImageResult, Limits, Rgba,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    error::{DecodingError, LimitError, LimitErrorKind},
    metadata::Orientation,
};
use smallvec::SmallVec;
use std::{borrow::Cow, io::Cursor};

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

/// The most frames of an animation that are decoded; a longer one is refused.
pub const MAX_ANIMATION_FRAMES: usize = 1024;

/// The bounds every raster decode keeps to: no side longer than [`MAX_RASTER_SIDE`], which the
/// GPU atlas could not hold, and no more memory than the SVG rasteriser may take.
pub fn raster_limits() -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_image_width = Some(MAX_RASTER_SIDE);
    limits.max_image_height = Some(MAX_RASTER_SIDE);
    limits.max_alloc = Some(MAX_RASTER_BYTES);
    limits
}

/// Reads the header of `bytes` as `format`. An unreadable orientation tag is treated as "upright".
pub fn image_header(bytes: &[u8], format: ImageFormat) -> ImageResult<ImageHeader> {
    let bytes = single_ico_entry(bytes, format);
    let mut decoder = ImageReader::with_format(Cursor::new(&*bytes), format).into_decoder()?;
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
/// An ICO file is decoded from its largest entry, which is also the entry shown when that is a
/// PNG that is not RGBA, which the `image` crate refuses.
pub fn decode_image(
    bytes: &[u8],
    format: ImageFormat,
    limits: Limits,
) -> ImageResult<DynamicImage> {
    let bytes = single_ico_entry(bytes, format);
    match decode_oriented(&bytes, format, limits.clone()) {
        Err(error) if format == ImageFormat::Ico => match best_ico_entry(&bytes) {
            Some(entry) if entry.payload.starts_with(PNG_SIGNATURE) => {
                decode_oriented(entry.payload, ImageFormat::Png, limits)
                    .map(|image| DynamicImage::ImageRgba8(image.into_rgba8()))
            }
            _ => Err(error),
        },
        result => result,
    }
}

/// Decodes `bytes` as `format` into the BGRA frames the renderer draws: one for a still image, one
/// per frame for an animated GIF or WebP.
///
/// The decode keeps to [`raster_limits`]. An animation is refused, with no frames at all, when its
/// canvas is too large or when its frames together would take more memory than a still image may,
/// or when it has more than [`MAX_ANIMATION_FRAMES`] frames.
pub fn decode_bgra_frames(bytes: &[u8], format: ImageFormat) -> ImageResult<SmallVec<[Frame; 1]>> {
    let limits = raster_limits();
    match format {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
            // `GifDecoder` starts with no limits at all.
            decoder.set_limits(limits.clone())?;
            let canvas = decoder.dimensions();
            collect_animation(decoder.into_frames(), format, canvas, limits)
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(Cursor::new(bytes))?;
            // The canvas is checked before a frame is allocated for it: `WebPDecoder` keeps no
            // limits of its own.
            decoder.set_limits(limits.clone())?;
            if decoder.has_animation() {
                decoder.set_background_color(Rgba([0, 0, 0, 0]))?;
                let canvas = decoder.dimensions();
                collect_animation(decoder.into_frames(), format, canvas, limits)
            } else {
                decode_still(bytes, format, limits)
            }
        }
        _ => decode_still(bytes, format, limits),
    }
}

fn decode_still(
    bytes: &[u8],
    format: ImageFormat,
    limits: Limits,
) -> ImageResult<SmallVec<[Frame; 1]>> {
    let mut image = decode_image(bytes, format, limits)?.into_rgba8();
    swap_red_and_blue(&mut image);
    Ok(SmallVec::from_elem(Frame::new(image), 1))
}

/// Every frame the decoder yields is a whole canvas, so the memory they take is known before one is
/// decoded and no partly decoded animation is ever returned.
fn collect_animation(
    frames: Frames<'_>,
    format: ImageFormat,
    (canvas_width, canvas_height): (u32, u32),
    mut budget: Limits,
) -> ImageResult<SmallVec<[Frame; 1]>> {
    // The decoder composites onto a canvas of its own.
    budget.reserve_buffer(canvas_width, canvas_height, ColorType::Rgba8)?;

    let mut decoded = SmallVec::new();
    for (index, frame) in frames.enumerate() {
        if index == MAX_ANIMATION_FRAMES {
            return Err(limit_error(LimitErrorKind::InsufficientMemory));
        }
        match frame {
            Ok(mut frame) => {
                budget.reserve_usize(frame.buffer().as_raw().len())?;
                swap_red_and_blue(frame.buffer_mut());
                decoded.push(frame);
            }
            // A frame over the limits means the file is too large, not that the frame is broken.
            Err(error @ ImageError::Limits(_)) => return Err(error),
            Err(error) => log::debug!("Skipping {format:?} frame due to decode error: {error}"),
        }
    }

    if decoded.is_empty() {
        return Err(ImageError::Decoding(DecodingError::new(
            format.into(),
            "all frames failed to decode",
        )));
    }
    Ok(decoded)
}

fn limit_error(kind: LimitErrorKind) -> ImageError {
    ImageError::Limits(LimitError::from_kind(kind))
}

fn swap_red_and_blue(image: &mut image::RgbaImage) {
    for pixel in image.chunks_exact_mut(4) {
        pixel.swap(0, 2);
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

struct IcoEntry<'a> {
    directory_entry: &'a [u8],
    payload: &'a [u8],
}

/// The entry an ICO viewer would show: the one with the greatest area, and among those the
/// greatest bit depth. `None` when the file has no entry that lies within it.
fn best_ico_entry(ico: &[u8]) -> Option<IcoEntry<'_>> {
    const HEADER_LEN: usize = 6;
    if ico.get(..4)? != [0, 0, 1, 0] {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]));
    (0..count)
        .filter_map(|ix| {
            let start = HEADER_LEN + ix * ICO_ENTRY_LEN;
            let directory_entry = ico.get(start..start + ICO_ENTRY_LEN)?;
            // A stored size of 0 means 256.
            let side = |stored: u8| if stored == 0 { 256 } else { u32::from(stored) };
            let area = side(directory_entry[0]) * side(directory_entry[1]);
            let bit_count = u16::from_le_bytes([directory_entry[6], directory_entry[7]]);
            let len = u32::from_le_bytes(directory_entry[8..12].try_into().ok()?) as usize;
            let offset = u32::from_le_bytes(directory_entry[12..16].try_into().ok()?) as usize;
            let payload = ico.get(offset..offset.checked_add(len)?)?;
            Some((
                (area, bit_count),
                IcoEntry {
                    directory_entry,
                    payload,
                },
            ))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, entry)| entry)
}

const ICO_ENTRY_LEN: usize = 16;

/// `bytes` as a directory holding only the entry [`best_ico_entry`] picks when it is an ICO: the
/// `image` crate would pick the entry with the greatest bit depth first, which shows a 16 x 16
/// icon with an alpha channel rather than a 256 x 256 one without.
fn single_ico_entry(bytes: &[u8], format: ImageFormat) -> Cow<'_, [u8]> {
    const SINGLE_ENTRY_PAYLOAD_OFFSET: u32 = 6 + ICO_ENTRY_LEN as u32;
    if format != ImageFormat::Ico {
        return Cow::Borrowed(bytes);
    }
    let Some(entry) = best_ico_entry(bytes) else {
        return Cow::Borrowed(bytes);
    };
    let mut single = Vec::with_capacity(SINGLE_ENTRY_PAYLOAD_OFFSET as usize + entry.payload.len());
    single.extend([0, 0, 1, 0, 1, 0]);
    single.extend(&entry.directory_entry[..12]);
    single.extend(SINGLE_ENTRY_PAYLOAD_OFFSET.to_le_bytes());
    single.extend(entry.payload);
    Cow::Owned(single)
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
    use image::{
        ImageEncoder as _, Rgb, RgbImage,
        codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    };

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

    fn rgba_png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    #[test]
    fn an_ico_shows_its_largest_entry_not_its_deepest() {
        let small_with_alpha = rgba_png(16, 16, [1, 2, 3, 255]);
        let large_without_alpha = png(&RgbImage::from_pixel(256, 256, Rgb([9, 8, 7])), None);
        // A stored size of 0 means 256.
        let ico = ico_with(&[
            (16, 16, 32, &small_with_alpha),
            (0, 0, 24, &large_without_alpha),
        ]);

        let by_image_crate = image::load_from_memory_with_format(&ico, ImageFormat::Ico).unwrap();
        assert_eq!(
            by_image_crate.width(),
            16,
            "the image crate ranks bit depth before area"
        );

        let decoded = decode_image(&ico, ImageFormat::Ico, Limits::default()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (256, 256));
        assert_eq!(decoded.to_rgba8().get_pixel(5, 5).0, [9, 8, 7, 255]);
        let header = image_header(&ico, ImageFormat::Ico).unwrap();
        assert_eq!((header.width, header.height), (256, 256));
    }

    #[test]
    fn an_ico_prefers_the_deeper_of_two_entries_of_the_same_size() {
        let shallow = rgba_png(32, 32, [1, 1, 1, 255]);
        let deep = rgba_png(32, 32, [2, 2, 2, 255]);
        let ico = ico_with(&[(32, 32, 24, &shallow), (32, 32, 32, &deep)]);
        let decoded = decode_image(&ico, ImageFormat::Ico, Limits::default()).unwrap();
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0, [2, 2, 2, 255]);
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

    /// A GIF whose canvas is `width` x `height` and whose frames are each one red pixel, so that it
    /// takes few bytes however large a canvas it claims. `frame_sizes` is the size each frame claims.
    fn gif_with_frame_sizes(width: u16, height: u16, frame_sizes: &[(u16, u16)]) -> Vec<u8> {
        let mut gif = b"GIF89a".to_vec();
        gif.extend(width.to_le_bytes());
        gif.extend(height.to_le_bytes());
        // A two-colour global palette: red and black.
        gif.extend([0x80, 0, 0, 0xFF, 0, 0, 0, 0, 0]);
        for (frame_width, frame_height) in frame_sizes {
            gif.extend([0x21, 0xF9, 0x04, 0x00, 0x0A, 0x00, 0x00, 0x00]);
            gif.extend([0x2C, 0, 0, 0, 0]);
            gif.extend(frame_width.to_le_bytes());
            gif.extend(frame_height.to_le_bytes());
            gif.push(0);
            gif.extend([0x02, 0x02, 0x44, 0x01, 0x00]);
        }
        gif.push(0x3B);
        gif
    }

    fn gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
        gif_with_frame_sizes(width, height, &vec![(1, 1); frames])
    }

    fn riff_chunk(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut chunk = fourcc.to_vec();
        chunk.extend((payload.len() as u32).to_le_bytes());
        chunk.extend(payload);
        if payload.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    /// An animated WebP whose canvas is `width` x `height` and whose `frames` frames are each one
    /// red pixel.
    fn animated_webp(width: u32, height: u32, frames: usize) -> Vec<u8> {
        let mut lossless = Vec::new();
        WebPEncoder::new_lossless(&mut lossless)
            .write_image(&[255, 0, 0, 255], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        // The still image's `VP8L` chunk, after the 12 byte RIFF header.
        let pixel = &lossless[12..];

        let three_bytes = |value: u32| value.to_le_bytes()[..3].to_vec();
        let mut extended_header = vec![0x12, 0, 0, 0];
        extended_header.extend(three_bytes(width - 1));
        extended_header.extend(three_bytes(height - 1));

        let mut body = b"WEBP".to_vec();
        body.extend(riff_chunk(b"VP8X", &extended_header));
        body.extend(riff_chunk(b"ANIM", &[0; 6]));
        for _ in 0..frames {
            let mut frame = vec![0; 6];
            frame.extend(three_bytes(0));
            frame.extend(three_bytes(0));
            frame.extend(three_bytes(10));
            frame.push(0);
            frame.extend(pixel);
            body.extend(riff_chunk(b"ANMF", &frame));
        }
        let mut webp = b"RIFF".to_vec();
        webp.extend((body.len() as u32).to_le_bytes());
        webp.extend(body);
        webp
    }

    fn limit_kind(result: ImageResult<SmallVec<[Frame; 1]>>) -> LimitErrorKind {
        match result {
            Err(ImageError::Limits(error)) => error.kind(),
            Err(other) => panic!("expected a limit error, got {other}"),
            Ok(frames) => panic!("expected a limit error, decoded {} frames", frames.len()),
        }
    }

    #[test]
    fn an_animated_gif_decodes_to_bgra_frames() {
        let frames = decode_bgra_frames(&gif(2, 2, 3), ImageFormat::Gif).unwrap();
        assert_eq!(frames.len(), 3);
        // Red, as blue-green-red-alpha.
        assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn a_gif_wider_than_the_raster_limit_is_refused_before_it_is_decoded() {
        let wide = gif(MAX_RASTER_SIDE as u16 + 1, 1, 1);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&wide, ImageFormat::Gif)),
            LimitErrorKind::DimensionError
        ));
    }

    #[test]
    fn a_gif_whose_canvas_alone_is_too_large_to_hold_is_refused() {
        // 16000 x 16000 x 4 bytes is a gigabyte, from a header and one pixel.
        let bomb = gif(16_000, 16_000, 1);
        assert!(bomb.len() < 100);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&bomb, ImageFormat::Gif)),
            LimitErrorKind::InsufficientMemory
        ));
    }

    #[test]
    fn a_gif_whose_frames_together_exceed_the_budget_yields_no_frames() {
        // Each frame is 4 MiB on a 1024 x 1024 canvas; 100 of them are 400 MiB, from 2 KB of file.
        let many = gif(1024, 1024, 100);
        assert!(many.len() < 4096);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&many, ImageFormat::Gif)),
            LimitErrorKind::InsufficientMemory
        ));
    }

    #[test]
    fn a_gif_frame_larger_than_the_raster_limit_refuses_the_whole_animation() {
        // The canvas is one pixel; the second frame claims 20000 x 20000 and is not skipped as if it
        // were a damaged frame.
        let bomb = gif_with_frame_sizes(1, 1, &[(1, 1), (20_000, 20_000), (1, 1)]);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&bomb, ImageFormat::Gif)),
            LimitErrorKind::DimensionError
        ));
    }

    #[test]
    fn a_gif_is_refused_past_the_frame_cap_and_decoded_at_it() {
        let at_the_cap = decode_bgra_frames(&gif(1, 1, MAX_ANIMATION_FRAMES), ImageFormat::Gif);
        assert_eq!(at_the_cap.unwrap().len(), MAX_ANIMATION_FRAMES);

        let past_the_cap = gif(1, 1, MAX_ANIMATION_FRAMES + 1);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&past_the_cap, ImageFormat::Gif)),
            LimitErrorKind::InsufficientMemory
        ));
    }

    #[test]
    fn an_animated_webp_decodes_to_bgra_frames() {
        let frames = decode_bgra_frames(&animated_webp(2, 2, 3), ImageFormat::WebP).unwrap();
        assert_eq!(frames.len(), 3);
        // Red, as blue-green-red-alpha; compositing may leave the channel a step short of full.
        let [blue, green, red, alpha] = frames[0].buffer().get_pixel(0, 0).0;
        assert_eq!((blue, green, alpha), (0, 0, 255));
        assert!(red >= 254, "red channel was {red}");
    }

    #[test]
    fn an_animated_webp_wider_than_the_raster_limit_is_refused() {
        let wide = animated_webp(MAX_RASTER_SIDE + 1, 1, 1);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&wide, ImageFormat::WebP)),
            LimitErrorKind::DimensionError
        ));
    }

    #[test]
    fn an_animated_webp_canvas_too_large_to_hold_is_refused() {
        let bomb = animated_webp(16_000, 16_000, 1);
        assert!(bomb.len() < 200);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&bomb, ImageFormat::WebP)),
            LimitErrorKind::InsufficientMemory
        ));
    }

    #[test]
    fn an_animated_webp_whose_frames_together_exceed_the_budget_yields_no_frames() {
        let many = animated_webp(1024, 1024, 100);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&many, ImageFormat::WebP)),
            LimitErrorKind::InsufficientMemory
        ));
    }

    #[test]
    fn an_animated_webp_is_refused_past_the_frame_cap() {
        let past_the_cap = animated_webp(1, 1, MAX_ANIMATION_FRAMES + 1);
        assert!(matches!(
            limit_kind(decode_bgra_frames(&past_the_cap, ImageFormat::WebP)),
            LimitErrorKind::InsufficientMemory
        ));
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
