//! The one place gpui decodes raster images, so that every caller gets the same bounds, the
//! same EXIF orientation and the same tolerance for icon files.

use crate::{MAX_RASTER_BYTES, MAX_RASTER_SIDE};
use image::{
    AnimationDecoder as _, ColorType, DynamicImage, ExtendedColorType, Frame, Frames,
    ImageDecoder as _, ImageError, ImageFormat, ImageReader, ImageResult, Limits, Rgba, RgbaImage,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    error::{DecodingError, LimitError, LimitErrorKind, ParameterError, ParameterErrorKind},
    metadata::Orientation,
};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    io::Cursor,
    sync::atomic::{AtomicBool, Ordering},
};
use web_time::{Duration, Instant};

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
///
/// No pixels are decoded and no side is refused however long, but what the decoder buffers for
/// the metadata it reads (a colour profile that PNG deflates, say) is held to
/// [`STREAMED_METADATA_BYTES`], not the `image` crate's 512 MiB default: a profile past it is
/// dropped, and the dimensions are still read.
pub fn image_header(bytes: &[u8], format: ImageFormat) -> ImageResult<ImageHeader> {
    let bytes = single_ico_entry(bytes, format);
    let mut reader = ImageReader::with_format(Cursor::new(&*bytes), format);
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(STREAMED_METADATA_BYTES as u64);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
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

/// The most pixels [`png_thumbnail`] streams through: what a raster [`MAX_RASTER_SIDE`] square
/// holds. Its work is linear in them, so this bounds the time a hostile header can cost.
const MAX_STREAMED_PIXELS: u64 = MAX_RASTER_SIDE as u64 * MAX_RASTER_SIDE as u64;

/// The longest side [`png_thumbnail`] streams, which bounds the per-column table and the row
/// buffers however thin the picture.
const MAX_STREAMED_SIDE: u32 = 1 << 20;

/// The largest `max_side` [`png_thumbnail`] accepts: its sums take 32 bytes per output pixel.
pub const MAX_THUMBNAIL_SIDE: u32 = 1024;

/// What the `png` crate may keep for the chunks it buffers (a palette, a colour profile).
const STREAMED_METADATA_BYTES: usize = 16 * 1024 * 1024;

/// How long [`png_thumbnail`] works before it gives up with [`ThumbnailError::TimedOut`]. A square
/// [`MAX_RASTER_SIDE`] picture streams in a few seconds; this is the ceiling for a slow machine,
/// not a budget to spend.
pub const THUMBNAIL_DEADLINE: Duration = Duration::from_secs(10);

/// How many pixels [`png_thumbnail`] streams between two looks at its cancel flag and the clock.
/// Counted in pixels, not rows, so the wait is about ten milliseconds however wide the rows are.
const PIXELS_BETWEEN_CHECKS: u64 = 1 << 20;

/// Why [`png_thumbnail`] gave no thumbnail.
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    /// The picture is not one that can be streamed: it is damaged or truncated, it declares more
    /// than the budget, or `max_side` is out of range.
    #[error(transparent)]
    Image(#[from] ImageError),
    /// The caller set the cancel flag; no thumbnail is wanted any more.
    #[error("the thumbnail was cancelled")]
    Cancelled,
    /// Streaming took longer than [`THUMBNAIL_DEADLINE`].
    #[error("the thumbnail took longer than {} seconds", THUMBNAIL_DEADLINE.as_secs())]
    TimedOut,
}

/// `(x_start, x_step, y_start, y_step)` of the seven Adam7 passes, in the order their rows are
/// stored.
const ADAM7_PASSES: [(u32, u32, u32, u32); 7] = [
    (0, 8, 0, 8),
    (4, 8, 0, 8),
    (0, 4, 4, 8),
    (2, 4, 0, 4),
    (0, 2, 2, 4),
    (1, 2, 0, 2),
    (0, 1, 1, 2),
];

/// A thumbnail of the first image of a PNG, at most `max_side` pixels on its longer side and never
/// larger than the picture itself, for a picture too large to decode whole.
///
/// The rows are inflated one at a time and averaged into the thumbnail as they arrive (alpha
/// weighted), so no raster is ever allocated: memory is the thumbnail's sums plus a few rows,
/// whatever the picture's size. Every PNG colour type and bit depth is read, and an Adam7
/// interlaced picture is placed pass by pass, and the thumbnail is upright by the picture's EXIF
/// orientation, as [`decode_image`] draws it. The picture may declare at most as many pixels as a
/// [`MAX_RASTER_SIDE`] square holds and no more than 2^20 on a side; a truncated or damaged file is
/// an error, not a partial thumbnail.
///
/// `max_side` must be between 1 and [`MAX_THUMBNAIL_SIDE`].
///
/// The work is linear in the pixels the header declares, and a damaged file is found only when the
/// stream reaches the damage, so the call looks at `cancel` and at [`THUMBNAIL_DEADLINE`] about
/// every million pixels (about ten milliseconds of work). It returns [`ThumbnailError::Cancelled`]
/// once `cancel` is set and [`ThumbnailError::TimedOut`] once the deadline has passed, with no
/// thumbnail.
pub fn png_thumbnail(
    bytes: &[u8],
    max_side: u32,
    cancel: &AtomicBool,
) -> Result<RgbaImage, ThumbnailError> {
    let deadline = Instant::now() + THUMBNAIL_DEADLINE;
    stream_png_thumbnail(bytes, max_side, || stop_requested(cancel, deadline))
}

fn stop_requested(cancel: &AtomicBool, deadline: Instant) -> Result<(), ThumbnailError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ThumbnailError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ThumbnailError::TimedOut)
    } else {
        Ok(())
    }
}

/// [`png_thumbnail`] with the stop decided by `checkpoint`, which is asked before any work and then
/// every [`PIXELS_BETWEEN_CHECKS`] pixels, and ends the stream by returning an error.
fn stream_png_thumbnail(
    bytes: &[u8],
    max_side: u32,
    mut checkpoint: impl FnMut() -> Result<(), ThumbnailError>,
) -> Result<RgbaImage, ThumbnailError> {
    checkpoint()?;
    if !(1..=MAX_THUMBNAIL_SIDE).contains(&max_side) {
        return Err(
            ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::Generic(
                format!(
                    "a PNG thumbnail side is between 1 and {MAX_THUMBNAIL_SIDE}, not {max_side}"
                ),
            )))
            .into(),
        );
    }
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: STREAMED_METADATA_BYTES,
        },
    );
    // Every row comes out as 8-bit grey, grey and alpha, RGB or RGBA.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_ignore_text_chunk(true);
    let mut reader = decoder.read_info().map_err(png_error)?;

    let info = reader.info();
    let (width, height, interlaced) = (info.width, info.height, info.interlaced);
    // An unreadable orientation tag is treated as "upright", as `image_header` does.
    let orientation = info
        .exif_metadata
        .as_deref()
        .and_then(Orientation::from_exif_chunk)
        .unwrap_or(Orientation::NoTransforms);
    if width > MAX_STREAMED_SIDE
        || height > MAX_STREAMED_SIDE
        || u64::from(width) * u64::from(height) > MAX_STREAMED_PIXELS
    {
        return Err(limit_error(LimitErrorKind::DimensionError).into());
    }
    let (color, depth) = reader.output_color_type();
    let samples = Samples::of(color, depth)?;

    let (thumbnail_width, thumbnail_height) = fit_within(width, height, max_side);
    let columns = AxisMap::new(width, thumbnail_width);
    let rows = AxisMap::new(height, thumbnail_height);
    let thumbnail_width = thumbnail_width as usize;
    let mut sums = vec![Sum::default(); thumbnail_width * thumbnail_height as usize];

    let mut pixels_since_check = 0;
    for placement in row_placements(width, height, interlaced) {
        if pixels_since_check >= PIXELS_BETWEEN_CHECKS {
            checkpoint()?;
            pixels_since_check = 0;
        }
        pixels_since_check += u64::from(placement.samples);
        let row = reader
            .next_row()
            .map_err(png_error)?
            .ok_or_else(|| png_decode_error("the image data ends before its last row"))?;
        let pixel_bytes = samples.bytes_per_pixel();
        if row.data().len() != placement.samples as usize * pixel_bytes {
            return Err(png_decode_error("a row is not the length its header gives").into());
        }
        let start = rows.cell_of[placement.y as usize] as usize * thumbnail_width;
        let sums_in_row = &mut sums[start..start + thumbnail_width];
        for (index, pixel) in row.data().chunks_exact(pixel_bytes).enumerate() {
            let x = placement.x_start + index as u32 * placement.x_step;
            sums_in_row[columns.cell_of[x as usize] as usize].add(samples.rgba(pixel));
        }
    }

    // The chunks after the last row are part of the file: damage there is damage, as it is when
    // the picture is decoded whole.
    reader.finish().map_err(png_error)?;

    let mut thumbnail = RgbaImage::new(thumbnail_width as u32, thumbnail_height);
    for (index, (pixel, sum)) in thumbnail.pixels_mut().zip(&sums).enumerate() {
        let covered = u64::from(columns.sizes[index % thumbnail_width])
            * u64::from(rows.sizes[index / thumbnail_width]);
        *pixel = sum.average(covered);
    }
    // Turning the thumbnail is the turning of the picture, at the cost of its few pixels.
    let mut thumbnail = DynamicImage::ImageRgba8(thumbnail);
    thumbnail.apply_orientation(orientation);
    Ok(thumbnail.into_rgba8())
}

/// `width` x `height` scaled to `max_side` on its longer side, never enlarged.
fn fit_within(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let longest = u64::from(width.max(height));
    if longest <= u64::from(max_side) {
        return (width, height);
    }
    let scaled = |side: u32| {
        let scaled = (u64::from(side) * u64::from(max_side) + longest / 2) / longest;
        scaled.max(1) as u32
    };
    (scaled(width), scaled(height))
}

/// How the positions along one side of a picture fall into the cells of its thumbnail: position
/// `p` of `side` is in cell `p * cells / side`, which keeps every cell's share within one of the
/// others'.
struct AxisMap {
    cell_of: Vec<u32>,
    /// How many positions fall into each cell.
    sizes: Vec<u32>,
}

impl AxisMap {
    fn new(side: u32, cells: u32) -> Self {
        let mut sizes = vec![0; cells as usize];
        let cell_of = (0..side)
            .map(|position| {
                let cell = (u64::from(position) * u64::from(cells) / u64::from(side)) as u32;
                sizes[cell as usize] += 1;
                cell
            })
            .collect();
        Self { cell_of, sizes }
    }
}

/// Where the next row the PNG decoder yields belongs in the picture.
struct RowPlacement {
    y: u32,
    x_start: u32,
    x_step: u32,
    /// How many pixels the row holds.
    samples: u32,
}

/// The placement of every row in the order the decoder yields them. The `png` crate exposes which
/// Adam7 pass a row is from only to its own deinterlacer, which needs the whole picture.
fn row_placements(width: u32, height: u32, interlaced: bool) -> impl Iterator<Item = RowPlacement> {
    let passes: &'static [(u32, u32, u32, u32)] = if interlaced {
        &ADAM7_PASSES
    } else {
        &[(0, 1, 0, 1)]
    };
    passes
        .iter()
        .flat_map(move |&(x_start, x_step, y_start, y_step)| {
            let samples = width.saturating_sub(x_start).div_ceil(x_step);
            let lines = if samples == 0 {
                0
            } else {
                height.saturating_sub(y_start).div_ceil(y_step)
            };
            (0..lines).map(move |line| RowPlacement {
                y: y_start + line * y_step,
                x_start,
                x_step,
                samples,
            })
        })
}

/// The pixel layout of an 8-bit row.
#[derive(Clone, Copy)]
enum Samples {
    Grey,
    GreyAlpha,
    Rgb,
    Rgba,
}

impl Samples {
    fn of(color: png::ColorType, depth: png::BitDepth) -> ImageResult<Self> {
        match (color, depth) {
            (png::ColorType::Grayscale, png::BitDepth::Eight) => Ok(Self::Grey),
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => Ok(Self::GreyAlpha),
            (png::ColorType::Rgb, png::BitDepth::Eight) => Ok(Self::Rgb),
            (png::ColorType::Rgba, png::BitDepth::Eight) => Ok(Self::Rgba),
            _ => Err(png_decode_error(
                "the decoder did not expand the pixels to 8 bits a channel",
            )),
        }
    }

    fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Grey => 1,
            Self::GreyAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }

    fn rgba(self, pixel: &[u8]) -> [u8; 4] {
        match self {
            Self::Grey => [pixel[0], pixel[0], pixel[0], 255],
            Self::GreyAlpha => [pixel[0], pixel[0], pixel[0], pixel[1]],
            Self::Rgb => [pixel[0], pixel[1], pixel[2], 255],
            Self::Rgba => [pixel[0], pixel[1], pixel[2], pixel[3]],
        }
    }
}

/// The alpha-weighted sum of the pixels that fall into one cell of a thumbnail. Each is kept in
/// 64 bits: a cell can hold every pixel of a 2^28 pixel picture.
#[derive(Clone, Copy, Default)]
struct Sum {
    red: u64,
    green: u64,
    blue: u64,
    alpha: u64,
}

impl Sum {
    fn add(&mut self, [red, green, blue, alpha]: [u8; 4]) {
        let alpha = u64::from(alpha);
        self.red += u64::from(red) * alpha;
        self.green += u64::from(green) * alpha;
        self.blue += u64::from(blue) * alpha;
        self.alpha += alpha;
    }

    /// The mean of `covered` pixels: colour weighted by alpha, so a clear pixel does not tint it.
    fn average(self, covered: u64) -> Rgba<u8> {
        if self.alpha == 0 {
            return Rgba([0; 4]);
        }
        let colour = |sum: u64| ((sum + self.alpha / 2) / self.alpha) as u8;
        Rgba([
            colour(self.red),
            colour(self.green),
            colour(self.blue),
            ((self.alpha + covered / 2) / covered) as u8,
        ])
    }
}

fn png_error(error: png::DecodingError) -> ImageError {
    match error {
        png::DecodingError::LimitsExceeded => limit_error(LimitErrorKind::InsufficientMemory),
        error => ImageError::Decoding(DecodingError::new(ImageFormat::Png.into(), error)),
    }
}

fn png_decode_error(message: &str) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormat::Png.into(), message))
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
                let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
                let mut frames = collect_animation(decoder.into_frames(), format, canvas, limits)?;
                orient_frames(&mut frames, orientation);
                Ok(frames)
            } else {
                decode_still(bytes, format, limits)
            }
        }
        _ => decode_still(bytes, format, limits),
    }
}

/// Turns every frame upright by `orientation`, as a still image is.
fn orient_frames(frames: &mut [Frame], orientation: Orientation) {
    if orientation == Orientation::NoTransforms {
        return;
    }
    for frame in frames {
        let upright = std::mem::replace(frame.buffer_mut(), RgbaImage::new(0, 0));
        let mut upright = DynamicImage::ImageRgba8(upright);
        upright.apply_orientation(orientation);
        *frame.buffer_mut() = upright.into_rgba8();
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
mod peak_allocation {
    //! A global allocator that tells a test how much memory a closure held at its peak. It counts
    //! only the calling thread, so tests running in parallel do not disturb one another.

    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    fn record(change: isize) {
        let _ = LIVE.try_with(|live| {
            live.set(live.get() + change);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
        });
    }

    struct CountingAllocator;

    // SAFETY: every call is forwarded to `System` with the arguments it was given; the counters
    // are only read and written, never used to decide what to allocate.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                record(layout.size() as isize);
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                record(layout.size() as isize);
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
            record(-(layout.size() as isize));
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let moved = unsafe { System.realloc(pointer, layout, new_size) };
            if !moved.is_null() {
                record(new_size as isize - layout.size() as isize);
            }
            moved
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    /// Runs `work` and returns its result with the most bytes it held at once beyond what was
    /// already allocated on this thread when it started.
    pub(super) fn peak_bytes<T>(work: impl FnOnce() -> T) -> (T, usize) {
        let start = LIVE.with(Cell::get);
        PEAK.with(|peak| peak.set(start));
        let result = work();
        let peak = PEAK.with(Cell::get);
        (result, (peak - start).max(0) as usize)
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

    /// A 1 x 1 grey PNG carrying a colour profile of `profile_len` zero bytes, which deflates to
    /// a few hundred kilobytes however long it claims to be.
    fn png_with_colour_profile(profile_len: usize) -> Vec<u8> {
        let mut info = png::Info::with_size(1, 1);
        info.color_type = png::ColorType::Grayscale;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = Some(Cow::Owned(vec![0; profile_len]));
        let mut bytes = Vec::new();
        png::Encoder::with_info(&mut bytes, info)
            .unwrap()
            .write_header()
            .unwrap()
            .write_image_data(&[0])
            .unwrap();
        bytes
    }

    #[test]
    fn a_header_probe_does_not_inflate_a_colour_profile_past_the_metadata_budget() {
        let profile_len = 8 * STREAMED_METADATA_BYTES;
        let bytes = png_with_colour_profile(profile_len);
        assert!(
            bytes.len() < profile_len / 100,
            "the profile must deflate well"
        );

        let (whole, decoded) = peak_allocation::peak_bytes(|| {
            decode_image(&bytes, ImageFormat::Png, Limits::no_limits()).unwrap()
        });
        drop(whole);
        assert!(
            decoded >= profile_len,
            "the probe saw only {decoded} bytes held to inflate {profile_len}"
        );

        let (header, probed) =
            peak_allocation::peak_bytes(|| image_header(&bytes, ImageFormat::Png));
        let header = header.unwrap();
        assert_eq!((header.width, header.height), (1, 1));
        assert!(
            probed < 2 * STREAMED_METADATA_BYTES,
            "reading the header held {probed} bytes for a {profile_len} byte profile"
        );
    }

    #[test]
    fn the_header_probe_limits_leave_every_format_its_dimensions() {
        let picture = DynamicImage::ImageRgb8(RgbImage::from_pixel(6, 4, Rgb([9, 8, 7])));
        let encoded = |format| {
            let mut bytes = Vec::new();
            picture
                .write_to(&mut Cursor::new(&mut bytes), format)
                .unwrap();
            bytes
        };
        let mut formats = vec![
            (ImageFormat::Png, encoded(ImageFormat::Png)),
            (ImageFormat::Jpeg, encoded(ImageFormat::Jpeg)),
            (ImageFormat::Gif, encoded(ImageFormat::Gif)),
            (ImageFormat::WebP, encoded(ImageFormat::WebP)),
            (ImageFormat::Bmp, encoded(ImageFormat::Bmp)),
            (ImageFormat::Tiff, encoded(ImageFormat::Tiff)),
            (ImageFormat::Tga, encoded(ImageFormat::Tga)),
            (ImageFormat::Qoi, encoded(ImageFormat::Qoi)),
        ];
        formats.push((
            ImageFormat::Ico,
            ico_with(&[(6, 4, 32, &encoded(ImageFormat::Png))]),
        ));
        for (format, bytes) in formats {
            let header = image_header(&bytes, format).unwrap_or_else(|error| {
                panic!("the {format:?} header is not read: {error}");
            });
            assert_eq!((header.width, header.height), (6, 4), "{format:?}");
        }

        // No side is refused, however long: the caller decides what it will decode.
        let header = image_header(&header_only_png(100_000, 100_000), ImageFormat::Png).unwrap();
        assert_eq!((header.width, header.height), (100_000, 100_000));
    }

    #[test]
    fn the_header_is_read_without_decoding_pixels() {
        // Reading the header of a PNG that declares 16000 x 16000 pixels must not try to allocate or
        // decode them.
        let header = image_header(&header_only_png(16000, 16000), ImageFormat::Png).unwrap();
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

    /// [`png_thumbnail`] that is never cancelled, for the tests of what it draws. A stop would be a
    /// failure of the test, not a result to match.
    fn streamed_thumbnail(bytes: &[u8], max_side: u32) -> ImageResult<RgbaImage> {
        png_thumbnail(bytes, max_side, &AtomicBool::new(false)).map_err(|error| match error {
            ThumbnailError::Image(error) => error,
            stopped => panic!("the thumbnail was stopped: {stopped}"),
        })
    }

    /// Appends a PNG chunk, with the CRC the format asks for.
    fn push_png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend((data.len() as u32).to_be_bytes());
        png.extend(kind);
        png.extend(data);
        png.extend(png_chunk_crc(kind, data).to_be_bytes());
    }

    /// An 8-bit grey PNG that stops after its header and an empty first pixel chunk, whatever size
    /// it declares.
    fn header_only_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = PNG_SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8, 0, 0, 0, 0]);
        push_png_chunk(&mut png, b"IHDR", &ihdr);
        push_png_chunk(&mut png, b"IDAT", &[]);
        png
    }

    /// Encodes `data` (the rows of the picture, packed as its colour type and depth say) as a PNG,
    /// after `configure` has set whatever else the picture needs.
    fn encode_png(
        (width, height): (u32, u32),
        color: png::ColorType,
        depth: png::BitDepth,
        configure: impl FnOnce(&mut png::Encoder<&mut Vec<u8>>),
        data: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        encoder.set_compression(png::Compression::Fast);
        configure(&mut encoder);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(data)
            .unwrap();
        bytes
    }

    fn grey_png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let data: Vec<u8> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| pixel(x, y))
            .collect();
        encode_png(
            (width, height),
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            |_| {},
            &data,
        )
    }

    /// An RGB PNG whose first half (left of the middle, or above it) is `first` and the rest
    /// `second`.
    fn two_tone_png(
        (width, height): (u32, u32),
        [first, second]: [[u8; 3]; 2],
        split_along_x: bool,
    ) -> Vec<u8> {
        let data: Vec<u8> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .flat_map(|(x, y)| {
                let in_first_half = if split_along_x {
                    x < width / 2
                } else {
                    y < height / 2
                };
                if in_first_half { first } else { second }
            })
            .collect();
        encode_png(
            (width, height),
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            |_| {},
            &data,
        )
    }

    /// An 8-bit grey Adam7 PNG, written by hand because the `png` encoder cannot interlace. Its pass
    /// table is spelled out here, from the PNG specification, rather than shared with the decoder.
    fn adam7_grey_png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        const PASSES: [(u32, u32, u32, u32); 7] = [
            (0, 8, 0, 8),
            (4, 8, 0, 8),
            (0, 4, 4, 8),
            (2, 4, 0, 4),
            (0, 2, 2, 4),
            (1, 2, 0, 2),
            (0, 1, 1, 2),
        ];
        let mut scanlines = Vec::new();
        for (x_start, x_step, y_start, y_step) in PASSES {
            if x_start >= width {
                continue;
            }
            for y in (y_start..height).step_by(y_step as usize) {
                // Filter type "none", then the pass's pixels in this row.
                scanlines.push(0);
                scanlines.extend(
                    (x_start..width)
                        .step_by(x_step as usize)
                        .map(|x| pixel(x, y)),
                );
            }
        }
        let mut png = PNG_SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(width.to_be_bytes());
        ihdr.extend(height.to_be_bytes());
        // 8 bits, grey, deflate, adaptive filtering, Adam7.
        ihdr.extend([8, 0, 0, 0, 1]);
        push_png_chunk(&mut png, b"IHDR", &ihdr);
        push_png_chunk(&mut png, b"IDAT", &stored_zlib(&scanlines));
        push_png_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// `data` as a zlib stream of stored (uncompressed) deflate blocks.
    fn stored_zlib(data: &[u8]) -> Vec<u8> {
        let mut zlib = vec![0x78, 0x01];
        let mut blocks = data.chunks(usize::from(u16::MAX)).peekable();
        while let Some(block) = blocks.next() {
            zlib.push(u8::from(blocks.peek().is_none()));
            zlib.extend((block.len() as u16).to_le_bytes());
            zlib.extend((!(block.len() as u16)).to_le_bytes());
            zlib.extend(block);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for byte in data {
            a = (a + u32::from(*byte)) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend(((b << 16) | a).to_be_bytes());
        zlib
    }

    const RED: [u8; 3] = [255, 0, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    #[test]
    fn a_wide_png_past_the_raster_side_streams_to_a_thumbnail() {
        let bytes = two_tone_png((20_000, 300), [RED, BLUE], true);
        assert!(
            decode_image(&bytes, ImageFormat::Png, raster_limits()).is_err(),
            "a PNG wider than MAX_RASTER_SIDE is refused by the whole-image decode"
        );

        let thumbnail = streamed_thumbnail(&bytes, 100).unwrap();
        assert_eq!(thumbnail.dimensions(), (100, 2));
        for y in 0..2 {
            assert_eq!(thumbnail.get_pixel(0, y).0, [255, 0, 0, 255]);
            assert_eq!(thumbnail.get_pixel(49, y).0, [255, 0, 0, 255]);
            assert_eq!(thumbnail.get_pixel(50, y).0, [0, 0, 255, 255]);
            assert_eq!(thumbnail.get_pixel(99, y).0, [0, 0, 255, 255]);
        }
    }

    #[test]
    fn a_tall_png_past_the_raster_side_streams_to_a_thumbnail() {
        let green = [0, 255, 0];
        let yellow = [255, 255, 0];
        let bytes = two_tone_png((300, 20_000), [green, yellow], false);
        assert!(decode_image(&bytes, ImageFormat::Png, raster_limits()).is_err());

        let thumbnail = streamed_thumbnail(&bytes, 100).unwrap();
        assert_eq!(thumbnail.dimensions(), (2, 100));
        for x in 0..2 {
            assert_eq!(thumbnail.get_pixel(x, 0).0, [0, 255, 0, 255]);
            assert_eq!(thumbnail.get_pixel(x, 49).0, [0, 255, 0, 255]);
            assert_eq!(thumbnail.get_pixel(x, 50).0, [255, 255, 0, 255]);
            assert_eq!(thumbnail.get_pixel(x, 99).0, [255, 255, 0, 255]);
        }
    }

    #[test]
    fn a_thumbnail_cell_is_the_mean_of_the_pixels_it_covers() {
        const ROWS: [[u8; 6]; 2] = [[0, 100, 50, 150, 10, 30], [20, 200, 70, 50, 90, 13]];
        let bytes = grey_png(6, 2, |x, y| ROWS[y as usize][x as usize]);
        let thumbnail = streamed_thumbnail(&bytes, 3).unwrap();
        assert_eq!(thumbnail.dimensions(), (3, 1));
        let cells: Vec<[u8; 4]> = thumbnail.pixels().map(|pixel| pixel.0).collect();
        assert_eq!(
            cells,
            [[80, 80, 80, 255], [80, 80, 80, 255], [36, 36, 36, 255]]
        );
    }

    #[test]
    fn a_clear_pixel_does_not_tint_the_mean_of_its_cell() {
        // An opaque red pixel beside a clear blue one averages to half-transparent red, not purple.
        let bytes = encode_png(
            (2, 1),
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            |_| {},
            &[255, 0, 0, 255, 0, 0, 255, 0],
        );
        let thumbnail = streamed_thumbnail(&bytes, 1).unwrap();
        assert_eq!(thumbnail.dimensions(), (1, 1));
        assert_eq!(thumbnail.get_pixel(0, 0).0, [255, 0, 0, 128]);
    }

    #[test]
    fn a_streamed_thumbnail_is_upright_by_the_exif_orientation() {
        let bytes = png(&landscape(), Some(exif_orientation(6)));
        let thumbnail = streamed_thumbnail(&bytes, 100).unwrap();
        assert_eq!(thumbnail.dimensions(), (2, 4));
        // The stored top-left corner of an orientation-6 picture is its top-right once upright.
        assert_eq!(thumbnail.get_pixel(1, 0).0, [255, 0, 0, 255]);
        assert_eq!(thumbnail.get_pixel(0, 0).0, [0, 0, 255, 255]);

        let unreadable = png(&landscape(), Some(exif_orientation(9)));
        assert_eq!(
            streamed_thumbnail(&unreadable, 100).unwrap().dimensions(),
            (4, 2)
        );
    }

    #[test]
    fn a_picture_already_within_the_side_is_not_enlarged() {
        let bytes = grey_png(4, 2, |x, y| (x * 10 + y) as u8);
        let thumbnail = streamed_thumbnail(&bytes, 100).unwrap();
        assert_eq!(thumbnail.dimensions(), (4, 2));
        assert_eq!(thumbnail.get_pixel(3, 1).0, [31, 31, 31, 255]);
    }

    #[test]
    fn every_png_colour_type_and_depth_is_read() {
        struct Case {
            name: &'static str,
            size: (u32, u32),
            color: png::ColorType,
            depth: png::BitDepth,
            data: &'static [u8],
            expected: &'static [[u8; 4]],
        }
        let cases = [
            Case {
                name: "1-bit grey",
                size: (8, 1),
                color: png::ColorType::Grayscale,
                depth: png::BitDepth::One,
                data: &[0b1010_0101],
                expected: &[
                    [255, 255, 255, 255],
                    [0, 0, 0, 255],
                    [255, 255, 255, 255],
                    [0, 0, 0, 255],
                    [0, 0, 0, 255],
                    [255, 255, 255, 255],
                    [0, 0, 0, 255],
                    [255, 255, 255, 255],
                ],
            },
            Case {
                name: "grey with alpha",
                size: (2, 1),
                color: png::ColorType::GrayscaleAlpha,
                depth: png::BitDepth::Eight,
                data: &[10, 20, 200, 255],
                expected: &[[10, 10, 10, 20], [200, 200, 200, 255]],
            },
            Case {
                name: "16-bit RGB",
                size: (1, 1),
                color: png::ColorType::Rgb,
                depth: png::BitDepth::Sixteen,
                data: &[0xFF, 0xFF, 0x80, 0x00, 0x00, 0x00],
                expected: &[[255, 128, 0, 255]],
            },
            Case {
                name: "palette with transparency",
                size: (3, 1),
                color: png::ColorType::Indexed,
                depth: png::BitDepth::Eight,
                data: &[0, 1, 2],
                expected: &[[255, 0, 0, 255], [0, 255, 0, 128], [0, 0, 255, 255]],
            },
        ];
        for case in cases {
            let bytes = encode_png(
                case.size,
                case.color,
                case.depth,
                |encoder| {
                    if case.color == png::ColorType::Indexed {
                        encoder.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255]);
                        encoder.set_trns(vec![255, 128]);
                    }
                },
                case.data,
            );
            let thumbnail = streamed_thumbnail(&bytes, 8).unwrap();
            let pixels: Vec<[u8; 4]> = thumbnail.pixels().map(|pixel| pixel.0).collect();
            assert_eq!(pixels, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn an_interlaced_png_is_placed_pass_by_pass() {
        let pixel = |x: u32, y: u32| ((x * 7 + y * 13) % 251) as u8;
        // Sizes that leave passes empty or short as well as ones that fill every pass.
        let sizes = [
            (1, 1),
            (2, 3),
            (5, 1),
            (1, 9),
            (8, 8),
            (9, 17),
            (13, 11),
            (40, 3),
        ];
        for (width, height) in sizes {
            let interlaced = adam7_grey_png(width, height, pixel);
            let plain = grey_png(width, height, pixel);

            let decoded = image::load_from_memory(&interlaced).unwrap().to_luma8();
            for (x, y, grey) in decoded.enumerate_pixels() {
                assert_eq!(
                    grey.0[0],
                    pixel(x, y),
                    "fixture {width}x{height} at {x},{y}"
                );
            }

            for side in [width.max(height), 4, 2, 1] {
                assert_eq!(
                    streamed_thumbnail(&interlaced, side).unwrap(),
                    streamed_thumbnail(&plain, side).unwrap(),
                    "{width}x{height} within {side}"
                );
            }
        }
    }

    #[test]
    fn a_truncated_or_damaged_png_is_an_error_not_a_panic() {
        let bytes = two_tone_png((64, 64), [RED, BLUE], true);
        for cut in [0, 4, PNG_SIGNATURE.len(), 20, 33, bytes.len() / 2] {
            assert!(
                matches!(
                    streamed_thumbnail(&bytes[..cut], 16),
                    Err(ImageError::Decoding(_))
                ),
                "cut at {cut} of {}",
                bytes.len()
            );
        }
        for cut in 0..bytes.len() {
            let _ = streamed_thumbnail(&bytes[..cut], 16);
        }

        let mut damaged = bytes;
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0xFF;
        assert!(streamed_thumbnail(&damaged, 16).is_err());
        assert!(streamed_thumbnail(b"not a png at all", 16).is_err());
    }

    #[test]
    fn every_frame_of_an_animation_is_turned_upright() {
        let mut frame = RgbaImage::from_pixel(4, 2, Rgba([0, 0, 255, 255]));
        frame.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let mut frames = [Frame::new(frame.clone()), Frame::new(frame)];
        orient_frames(&mut frames, Orientation::NoTransforms);
        assert_eq!(frames[0].buffer().dimensions(), (4, 2));

        orient_frames(&mut frames, Orientation::Rotate90);
        for frame in &frames {
            assert_eq!(frame.buffer().dimensions(), (2, 4));
            // The red corner moved from the top-left to the top-right.
            assert_eq!(frame.buffer().get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
            assert_eq!(frame.buffer().get_pixel(0, 0), &Rgba([0, 0, 255, 255]));
        }
    }

    #[test]
    fn damage_after_the_last_row_is_an_error_too() {
        let mut bytes = two_tone_png((64, 64), [RED, BLUE], true);
        assert!(streamed_thumbnail(&bytes, 16).is_ok());
        // The last byte is the checksum of the closing chunk.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(matches!(
            streamed_thumbnail(&bytes, 16),
            Err(ImageError::Decoding(_))
        ));
    }

    #[test]
    fn a_png_declaring_more_than_the_budget_is_refused_from_its_header() {
        let too_many_pixels = header_only_png(100_000, 100_000);
        let too_long = header_only_png(MAX_STREAMED_SIDE + 1, 1);
        for header in [too_many_pixels, too_long] {
            assert!(matches!(
                streamed_thumbnail(&header, 64),
                Err(ImageError::Limits(error)) if matches!(error.kind(), LimitErrorKind::DimensionError)
            ));
        }

        // The largest header a PNG can state is refused before anything is sized from it.
        let largest = header_only_png(i32::MAX as u32, i32::MAX as u32);
        assert!(matches!(
            streamed_thumbnail(&largest, 64),
            Err(ImageError::Limits(_))
        ));

        // A square raster's worth of pixels is within the budget, so it is read, and found to hold
        // no pixels.
        let at_the_budget = header_only_png(MAX_RASTER_SIDE, MAX_RASTER_SIDE);
        assert!(matches!(
            streamed_thumbnail(&at_the_budget, 64),
            Err(ImageError::Decoding(_))
        ));
    }

    #[test]
    fn a_thumbnail_side_outside_the_supported_range_is_refused() {
        let bytes = grey_png(2, 2, |_, _| 0);
        for side in [0, MAX_THUMBNAIL_SIDE + 1] {
            assert!(matches!(
                streamed_thumbnail(&bytes, side),
                Err(ImageError::Parameter(_))
            ));
        }
        assert!(streamed_thumbnail(&bytes, MAX_THUMBNAIL_SIDE).is_ok());
    }

    /// A grey PNG of 16.7 million pixels: sixteen looks at the cancel flag and the clock.
    fn long_stream() -> (Vec<u8>, u64) {
        (grey_png(4096, 4096, |x, y| (x ^ y) as u8), 4096 * 4096)
    }

    /// `stream_png_thumbnail` with `stop` asked at every checkpoint, and how often it was asked.
    fn stream_counting_checkpoints(
        bytes: &[u8],
        mut stop: impl FnMut(usize) -> Result<(), ThumbnailError>,
    ) -> (Result<RgbaImage, ThumbnailError>, usize) {
        let mut asked = 0;
        let result = stream_png_thumbnail(bytes, 64, || {
            asked += 1;
            stop(asked)
        });
        (result, asked)
    }

    #[test]
    fn a_long_stream_is_asked_whether_to_go_on_once_per_million_pixels() {
        let (bytes, pixels) = long_stream();
        let (result, asked) = stream_counting_checkpoints(&bytes, |_| Ok(()));
        result.unwrap();
        assert_eq!(asked as u64, 1 + (pixels - 1) / PIXELS_BETWEEN_CHECKS);
    }

    #[test]
    fn a_stream_cancelled_midway_stops_at_that_check() {
        let (bytes, _) = long_stream();
        let cancel = AtomicBool::new(false);
        let far_deadline = Instant::now() + Duration::from_secs(3600);
        let (result, asked) = stream_counting_checkpoints(&bytes, |asked| {
            if asked == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
            stop_requested(&cancel, far_deadline)
        });
        assert!(matches!(result, Err(ThumbnailError::Cancelled)));
        assert_eq!(asked, 3, "the stream went on after it was cancelled");
    }

    #[test]
    fn a_cancelled_thumbnail_does_no_work() {
        let cancelled = AtomicBool::new(true);
        let (bytes, _) = long_stream();
        for input in [&bytes[..], b"not a png at all"] {
            assert!(matches!(
                png_thumbnail(input, 64, &cancelled),
                Err(ThumbnailError::Cancelled)
            ));
        }
    }

    #[test]
    fn a_stream_past_its_deadline_is_stopped_at_its_next_check() {
        let (bytes, _) = long_stream();
        let never_cancelled = AtomicBool::new(false);
        let now = Instant::now();
        let (result, asked) =
            stream_counting_checkpoints(&bytes, |_| stop_requested(&never_cancelled, now));
        assert!(matches!(result, Err(ThumbnailError::TimedOut)));
        assert_eq!(asked, 1);
    }

    #[test]
    fn a_cancel_outranks_a_deadline_and_a_live_flag_does_not_stop_a_thumbnail() {
        let now = Instant::now();
        assert!(matches!(
            stop_requested(&AtomicBool::new(true), now),
            Err(ThumbnailError::Cancelled)
        ));
        let live = AtomicBool::new(false);
        stop_requested(&live, now + Duration::from_secs(60)).unwrap();
        let picture = png_thumbnail(&grey_png(2, 2, |_, _| 7), 2, &live).unwrap();
        assert_eq!(picture.dimensions(), (2, 2));
    }

    fn animated_png(canvas: (u32, u32), frames: &[((u32, u32), u8)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, canvas.0, canvas.1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(frames.len() as u32, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        for ((width, height), grey) in frames {
            writer.set_frame_dimension(*width, *height).unwrap();
            writer
                .write_image_data(&vec![*grey; (width * height) as usize])
                .unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn an_animated_png_is_thumbnailed_from_its_first_frame() {
        let bytes = animated_png((4, 4), &[((4, 4), 10), ((4, 4), 200)]);
        let thumbnail = streamed_thumbnail(&bytes, 4).unwrap();
        assert_eq!(thumbnail.dimensions(), (4, 4));
        assert!(thumbnail.pixels().all(|pixel| pixel.0 == [10, 10, 10, 255]));
    }

    #[test]
    fn an_animated_png_whose_first_frame_is_smaller_than_the_canvas_is_an_error() {
        // The decoder enforces the APNG rule that the default image covers the canvas; a thumbnail
        // is never drawn from part of it.
        let bytes = animated_png((4, 4), &[((2, 2), 10), ((4, 4), 200)]);
        assert!(matches!(
            streamed_thumbnail(&bytes, 4),
            Err(ImageError::Decoding(_))
        ));
    }

    /// Asserts that streaming `bytes` to a thumbnail holds a small fraction of the raster it
    /// describes, while decoding it whole holds at least the raster.
    fn assert_streaming_is_bounded(bytes: &[u8], raster_bytes: usize) {
        let (decoded, whole) = peak_allocation::peak_bytes(|| {
            decode_image(bytes, ImageFormat::Png, Limits::no_limits()).unwrap()
        });
        drop(decoded);
        assert!(
            whole >= raster_bytes,
            "the probe saw only {whole} bytes held to decode {raster_bytes}"
        );

        let (thumbnail, streamed) = peak_allocation::peak_bytes(|| streamed_thumbnail(bytes, 100));
        thumbnail.unwrap();
        assert!(
            streamed < raster_bytes / 8,
            "streaming held {streamed} bytes for a {raster_bytes} byte raster"
        );
    }

    #[test]
    fn streaming_a_wide_png_never_holds_its_raster() {
        let bytes = two_tone_png((20_000, 300), [RED, BLUE], true);
        assert_streaming_is_bounded(&bytes, 20_000 * 300 * 3);
    }

    #[test]
    fn streaming_a_tall_png_never_holds_its_raster() {
        let bytes = two_tone_png((300, 20_000), [RED, BLUE], false);
        assert_streaming_is_bounded(&bytes, 300 * 20_000 * 3);
    }

    #[test]
    fn streaming_an_interlaced_png_never_holds_its_raster() {
        let bytes = adam7_grey_png(4_000, 3_000, |x, y| (x ^ y) as u8);
        assert_streaming_is_bounded(&bytes, 4_000 * 3_000);
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
