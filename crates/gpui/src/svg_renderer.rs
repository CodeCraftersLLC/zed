use crate::{
    AssetSource, DevicePixels, IsZero, RenderImage, Result, SharedString, Size,
    swap_rgba_pa_to_bgra,
};
use image::Frame;
use resvg::tiny_skia::Pixmap;
use smallvec::SmallVec;
use std::{
    hash::Hash,
    sync::{Arc, LazyLock, OnceLock},
};

#[cfg(target_os = "macos")]
const EMOJI_FONT_FAMILIES: &[&str] = &["Apple Color Emoji", ".AppleColorEmojiUI"];

#[cfg(target_os = "windows")]
const EMOJI_FONT_FAMILIES: &[&str] = &["Segoe UI Emoji", "Segoe UI Symbol"];

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
const EMOJI_FONT_FAMILIES: &[&str] = &[
    "Noto Color Emoji",
    "Emoji One",
    "Twitter Color Emoji",
    "JoyPixels",
];

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
)))]
const EMOJI_FONT_FAMILIES: &[&str] = &[];

fn is_emoji_presentation(c: char) -> bool {
    static EMOJI_PRESENTATION_REGEX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("\\p{Emoji_Presentation}").unwrap());
    let mut buf = [0u8; 4];
    EMOJI_PRESENTATION_REGEX.is_match(c.encode_utf8(&mut buf))
}

fn font_has_char(db: &usvg::fontdb::Database, id: usvg::fontdb::ID, ch: char) -> bool {
    db.with_face_data(id, |font_data, face_index| {
        ttf_parser::Face::parse(font_data, face_index)
            .ok()
            .and_then(|face| face.glyph_index(ch))
            .is_some()
    })
    .unwrap_or(false)
}

fn select_emoji_font(
    ch: char,
    fonts: &[usvg::fontdb::ID],
    db: &usvg::fontdb::Database,
    families: &[&str],
) -> Option<usvg::fontdb::ID> {
    for family_name in families {
        let query = usvg::fontdb::Query {
            families: &[usvg::fontdb::Family::Name(family_name)],
            weight: usvg::fontdb::Weight(400),
            stretch: usvg::fontdb::Stretch::Normal,
            style: usvg::fontdb::Style::Normal,
        };

        let Some(id) = db.query(&query) else {
            continue;
        };

        if fonts.contains(&id) || !font_has_char(db, id, ch) {
            continue;
        }

        return Some(id);
    }

    None
}

/// An SVG is untrusted input: it may name `/etc/passwd`, a UNC share, a `file:` or
/// `http:` URL in an `<image href>`. Only inline `data:` images decode; every other
/// href resolves to nothing instead of being read from disk or the network.
fn inline_only_image_href_resolver() -> usvg::ImageHrefResolver<'static> {
    usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(|_, _| None),
    }
}

/// When rendering SVGs, we render them at twice the size to get a higher-quality result.
pub const SMOOTH_SVG_SCALE_FACTOR: f32 = 2.;

/// The longest side, in pixels, of an SVG raster. The GPU atlas cannot hold an image larger than
/// this, so anything bigger would be painted as nothing.
pub const MAX_RASTER_SIDE: u32 = 16_384;

/// The most memory a raster may take; an image within `MAX_RASTER_SIDE` on both sides can still be
/// gigabytes.
pub(crate) const MAX_RASTER_BYTES: u64 = 256 * 1024 * 1024;

/// Why an SVG could not be rasterised.
#[derive(Debug, thiserror::Error)]
pub enum SvgRenderError {
    /// The SVG could not be parsed or has no drawable size.
    #[error(transparent)]
    Svg(#[from] usvg::Error),
    /// The raster would exceed the size limit.
    #[error("{width} × {height} px is too large to draw")]
    TooLarge {
        /// The width the raster would have had.
        width: u32,
        /// The height the raster would have had.
        height: u32,
    },
}

#[derive(Clone, PartialEq, Hash, Eq)]
#[expect(missing_docs)]
pub struct RenderSvgParams {
    pub path: SharedString,
    pub size: Size<DevicePixels>,
}

#[derive(Clone)]
/// A struct holding everything necessary to render SVGs.
pub struct SvgRenderer {
    asset_source: Arc<dyn AssetSource>,
    usvg_options: Arc<usvg::Options<'static>>,
}

/// The size in which to render the SVG.
pub enum SvgSize {
    /// An absolute size in device pixels.
    Size(Size<DevicePixels>),
    /// A scaling factor to apply to the size provided by the SVG.
    ScaleFactor(f32),
}

impl SvgRenderer {
    /// Creates a new SVG renderer with the provided asset source.
    pub fn new(asset_source: Arc<dyn AssetSource>) -> Self {
        static SYSTEM_FONT_DB: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        });

        // Build the enriched font DB lazily on first SVG render rather than
        // eagerly at construction time. This avoids the expensive deep-clone
        // of the system font database for code paths that never render SVGs
        // (e.g. tests).
        let enriched_fontdb: Arc<OnceLock<Arc<usvg::fontdb::Database>>> = Arc::new(OnceLock::new());

        let default_font_resolver = usvg::FontResolver::default_font_selector();
        let font_resolver = Box::new({
            let asset_source = asset_source.clone();
            move |font: &usvg::Font, db: &mut Arc<usvg::fontdb::Database>| {
                if db.is_empty() {
                    let fontdb = enriched_fontdb.get_or_init(|| {
                        let mut db = (**SYSTEM_FONT_DB).clone();
                        load_bundled_fonts(&*asset_source, &mut db);
                        fix_generic_font_families(&mut db);
                        Arc::new(db)
                    });
                    *db = fontdb.clone();
                }
                if let Some(id) = default_font_resolver(font, db) {
                    return Some(id);
                }
                // fontdb doesn't recognize CSS system font keywords like "system-ui"
                // or "ui-sans-serif", so fall back to sans-serif before any face.
                let sans_query = usvg::fontdb::Query {
                    families: &[usvg::fontdb::Family::SansSerif],
                    ..Default::default()
                };
                db.query(&sans_query)
                    .or_else(|| db.faces().next().map(|f| f.id))
            }
        });
        let default_fallback_selection = usvg::FontResolver::default_fallback_selector();
        let fallback_selection = Box::new(
            move |ch: char, fonts: &[usvg::fontdb::ID], db: &mut Arc<usvg::fontdb::Database>| {
                if is_emoji_presentation(ch) {
                    if let Some(id) = select_emoji_font(ch, fonts, db.as_ref(), EMOJI_FONT_FAMILIES)
                    {
                        return Some(id);
                    }
                }

                default_fallback_selection(ch, fonts, db)
            },
        );
        let options = usvg::Options {
            font_resolver: usvg::FontResolver {
                select_font: font_resolver,
                select_fallback: fallback_selection,
            },
            image_href_resolver: inline_only_image_href_resolver(),
            ..Default::default()
        };
        Self {
            asset_source,
            usvg_options: Arc::new(options),
        }
    }

    /// Renders the given bytes into an image buffer. A raster larger than [`MAX_RASTER_SIDE`]
    /// is refused as [`usvg::Error::InvalidSize`]; use [`Self::render_single_frame_bounded`] to
    /// tell that apart from an unparsable SVG.
    pub fn render_single_frame(
        &self,
        bytes: &[u8],
        scale_factor: f32,
    ) -> Result<Arc<RenderImage>, usvg::Error> {
        self.render_single_frame_bounded(bytes, scale_factor, MAX_RASTER_SIDE)
            .map_err(|error| match error {
                SvgRenderError::Svg(error) => error,
                SvgRenderError::TooLarge { .. } => usvg::Error::InvalidSize,
            })
    }

    /// Renders the given bytes into an image buffer, refusing, before allocating anything, a raster
    /// with a side longer than `max_side` or more than 256 MiB of pixels.
    pub fn render_single_frame_bounded(
        &self,
        bytes: &[u8],
        scale_factor: f32,
        max_side: u32,
    ) -> Result<Arc<RenderImage>, SvgRenderError> {
        self.render_pixmap(
            bytes,
            SvgSize::ScaleFactor(scale_factor * SMOOTH_SVG_SCALE_FACTOR),
            Some(max_side),
        )
        .map(|pixmap| {
            let mut buffer =
                image::ImageBuffer::from_raw(pixmap.width(), pixmap.height(), pixmap.take())
                    .unwrap();

            for pixel in buffer.chunks_exact_mut(4) {
                swap_rgba_pa_to_bgra(pixel);
            }

            let mut image = RenderImage::new(SmallVec::from_const([Frame::new(buffer)]));
            image.scale_factor = SMOOTH_SVG_SCALE_FACTOR;
            Arc::new(image)
        })
    }

    pub(crate) fn render_alpha_mask(
        &self,
        params: &RenderSvgParams,
        bytes: Option<&[u8]>,
    ) -> Result<Option<(Size<DevicePixels>, Vec<u8>)>> {
        anyhow::ensure!(!params.size.is_zero(), "can't render at a zero size");

        let render_pixmap = |bytes| {
            let pixmap = self.render_pixmap(bytes, SvgSize::Size(params.size), None)?;

            // Convert the pixmap's pixels into an alpha mask.
            let size = Size::new(
                DevicePixels(pixmap.width() as i32),
                DevicePixels(pixmap.height() as i32),
            );
            let alpha_mask = pixmap
                .pixels()
                .iter()
                .map(|p| p.alpha())
                .collect::<Vec<_>>();

            Ok(Some((size, alpha_mask)))
        };

        if let Some(bytes) = bytes {
            render_pixmap(bytes)
        } else if let Some(bytes) = self.asset_source.load(&params.path)? {
            render_pixmap(&bytes)
        } else {
            Ok(None)
        }
    }

    fn render_pixmap(
        &self,
        bytes: &[u8],
        size: SvgSize,
        max_side: Option<u32>,
    ) -> Result<Pixmap, SvgRenderError> {
        let tree = usvg::Tree::from_data(bytes, &self.usvg_options)?;
        let svg_size = tree.size();
        let scale = match size {
            SvgSize::Size(size) => size.width.0 as f32 / svg_size.width(),
            SvgSize::ScaleFactor(scale) => scale,
        };
        let width = (svg_size.width() * scale) as u32;
        let height = (svg_size.height() * scale) as u32;
        if let Some(max_side) = max_side
            && (width > max_side
                || height > max_side
                || u64::from(width) * u64::from(height) * 4 > MAX_RASTER_BYTES)
        {
            return Err(SvgRenderError::TooLarge { width, height });
        }

        // Render the SVG to a pixmap with the specified width and height.
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(width, height).ok_or(usvg::Error::InvalidSize)?;

        let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);

        resvg::render(&tree, transform, &mut pixmap.as_mut());

        Ok(pixmap)
    }
}

fn load_bundled_fonts(asset_source: &dyn AssetSource, db: &mut usvg::fontdb::Database) {
    let font_paths = [
        "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
        "fonts/lilex/Lilex-Regular.ttf",
    ];
    for path in font_paths {
        match asset_source.load(path) {
            Ok(Some(data)) => db.load_font_data(data.into_owned()),
            Ok(None) => log::warn!("Bundled font not found: {path}"),
            Err(error) => log::warn!("Failed to load bundled font {path}: {error}"),
        }
    }
}

// fontdb defaults generic families to Microsoft fonts ("Arial", "Times New Roman")
// which aren't installed on most Linux systems. fontconfig normally overrides these,
// but when it fails the defaults remain and all generic family queries return None.
fn fix_generic_font_families(db: &mut usvg::fontdb::Database) {
    use usvg::fontdb::{Family, Query};

    let families_and_fallbacks: &[(Family<'_>, &str)] = &[
        (Family::SansSerif, "IBM Plex Sans"),
        // No serif font bundled; use sans-serif as best available fallback.
        (Family::Serif, "IBM Plex Sans"),
        (Family::Monospace, "Lilex"),
        (Family::Cursive, "IBM Plex Sans"),
        (Family::Fantasy, "IBM Plex Sans"),
    ];

    for (family, fallback_name) in families_and_fallbacks {
        let query = Query {
            families: &[*family],
            ..Default::default()
        };
        if db.query(&query).is_none() {
            match family {
                Family::SansSerif => db.set_sans_serif_family(*fallback_name),
                Family::Serif => db.set_serif_family(*fallback_name),
                Family::Monospace => db.set_monospace_family(*fallback_name),
                Family::Cursive => db.set_cursive_family(*fallback_name),
                Family::Fantasy => db.set_fantasy_family(*fallback_name),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usvg::fontdb::{Database, Family, Query};

    const IBM_PLEX_REGULAR: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    const LILEX_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf");

    /// Removes the files it created, even when an assertion unwinds.
    struct TempFiles(Vec<std::path::PathBuf>);

    impl Drop for TempFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                std::fs::remove_file(path).ok();
            }
        }
    }

    fn solid_red_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("PNG encodes");
        bytes.into_inner()
    }

    fn opaque_pixel_count(href: &str) -> usize {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><image href="{href}" width="8" height="8"/></svg>"#
        );
        let renderer = SvgRenderer::new(Arc::new(()));
        let pixmap = renderer
            .render_pixmap(svg.as_bytes(), SvgSize::ScaleFactor(1.), None)
            .expect("the SVG itself is valid");
        pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    #[test]
    fn svg_image_href_never_reads_the_filesystem() {
        let unique = format!("{}-{:?}", std::process::id(), std::thread::current().id())
            .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        let png = solid_red_png();
        let absolute = std::env::temp_dir().join(format!("gpui-svg-href-{unique}.png"));
        let relative = std::path::PathBuf::from(format!("gpui-svg-href-{unique}.png"));
        std::fs::write(&absolute, &png).expect("write absolute fixture");
        std::fs::write(&relative, &png).expect("write relative fixture");
        let _cleanup = TempFiles(vec![absolute.clone(), relative.clone()]);

        for href in [
            absolute.to_string_lossy().into_owned(),
            relative.to_string_lossy().into_owned(),
            format!("file://{}", absolute.to_string_lossy()),
            format!("http://127.0.0.1/{unique}.png"),
        ] {
            assert_eq!(opaque_pixel_count(&href), 0, "{href} must not be loaded");
        }
    }

    fn plain_svg(width: u32, height: u32) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}"><rect width="{width}" height="{height}" fill="red"/></svg>"#
        )
    }

    #[test]
    fn bounded_render_refuses_before_allocating() {
        let renderer = SvgRenderer::new(Arc::new(()));

        // 150,000 px tall: tiny-skia would allocate it happily and the GPU atlas could never hold it.
        let tall = plain_svg(100, 150_000);
        let Err(SvgRenderError::TooLarge { width, height }) =
            renderer.render_single_frame_bounded(tall.as_bytes(), 1.0, MAX_RASTER_SIDE)
        else {
            panic!("a 150,000 px tall SVG must be refused as too large");
        };
        assert_eq!((width, height), (200, 300_000));
        assert!(matches!(
            renderer.render_single_frame(tall.as_bytes(), 1.0),
            Err(usvg::Error::InvalidSize)
        ));

        // Within the side limit but over the memory limit: 12,000 x 12,000 x 4 bytes.
        let wide_and_tall = plain_svg(6_000, 6_000);
        assert!(matches!(
            renderer.render_single_frame_bounded(wide_and_tall.as_bytes(), 1.0, MAX_RASTER_SIDE),
            Err(SvgRenderError::TooLarge { .. })
        ));

        let small = plain_svg(10, 10);
        let image = renderer
            .render_single_frame_bounded(small.as_bytes(), 1.0, MAX_RASTER_SIDE)
            .expect("a small SVG renders");
        assert_eq!(
            image.size(0),
            crate::size(DevicePixels(20), DevicePixels(20))
        );
        assert!(matches!(
            renderer.render_single_frame_bounded(small.as_bytes(), 1.0, 10),
            Err(SvgRenderError::TooLarge {
                width: 20,
                height: 20
            })
        ));
        assert!(matches!(
            renderer.render_single_frame_bounded(b"not svg", 1.0, MAX_RASTER_SIDE),
            Err(SvgRenderError::Svg(_))
        ));
    }

    #[test]
    fn svg_image_href_still_decodes_inline_data_uris() {
        let encoded: String = solid_red_png()
            .iter()
            .map(|b| format!("%{b:02x}"))
            .collect();
        let href = format!("data:image/png,{encoded}");
        assert_eq!(opaque_pixel_count(&href), 64);
    }

    fn db_with_bundled_fonts() -> Database {
        let mut db = Database::new();
        db.load_font_data(IBM_PLEX_REGULAR.to_vec());
        db.load_font_data(LILEX_REGULAR.to_vec());
        db
    }

    #[test]
    fn test_is_emoji_presentation() {
        let cases = [
            ("a", false),
            ("Z", false),
            ("1", false),
            ("#", false),
            ("*", false),
            ("漢", false),
            ("中", false),
            ("カ", false),
            ("©", false),
            ("♥", false),
            ("😀", true),
            ("✅", true),
            ("🇺🇸", true),
            // SVG fallback is not cluster-aware yet
            ("©️", false),
            ("♥️", false),
            ("1️⃣", false),
        ];
        for (s, expected) in cases {
            assert_eq!(
                is_emoji_presentation(s.chars().next().unwrap()),
                expected,
                "for char {:?}",
                s
            );
        }
    }

    #[test]
    fn fix_generic_font_families_sets_all_families() {
        let mut db = db_with_bundled_fonts();
        fix_generic_font_families(&mut db);

        let families = [
            Family::SansSerif,
            Family::Serif,
            Family::Monospace,
            Family::Cursive,
            Family::Fantasy,
        ];

        for family in families {
            let query = Query {
                families: &[family],
                ..Default::default()
            };
            assert!(
                db.query(&query).is_some(),
                "Expected generic family {family:?} to resolve after fix_generic_font_families"
            );
        }
    }

    #[test]
    fn test_select_emoji_font_skips_family_without_glyph() {
        let mut db = db_with_bundled_fonts();

        let ibm_plex_sans = db
            .query(&usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::Name("IBM Plex Sans")],
                weight: usvg::fontdb::Weight(400),
                stretch: usvg::fontdb::Stretch::Normal,
                style: usvg::fontdb::Style::Normal,
            })
            .unwrap();
        let lilex = db
            .query(&usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::Name("Lilex")],
                weight: usvg::fontdb::Weight(400),
                stretch: usvg::fontdb::Stretch::Normal,
                style: usvg::fontdb::Style::Normal,
            })
            .unwrap();
        let selected = select_emoji_font('│', &[], &db, &["IBM Plex Sans", "Lilex"]).unwrap();

        assert_eq!(selected, lilex);
        assert!(!font_has_char(&db, ibm_plex_sans, '│'));
        assert!(font_has_char(&db, selected, '│'));
    }

    #[test]
    fn fix_generic_font_families_monospace_resolves_to_lilex() {
        let mut db = db_with_bundled_fonts();
        fix_generic_font_families(&mut db);

        let query = Query {
            families: &[Family::Monospace],
            ..Default::default()
        };
        let id = db.query(&query).expect("Monospace should resolve");
        let face = db.face(id).expect("Face should exist");
        assert!(
            face.families.iter().any(|(name, _)| name.contains("Lilex")),
            "Monospace should map to Lilex, got {:?}",
            face.families
        );
    }
}
