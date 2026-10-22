use fs::FakeFs;
use gpui::TestAppContext;
use project::Project;
use project::ProjectPath;
use project::image_store::*;
use serde_json::json;
use settings::SettingsStore;
use util::rel_path::rel_path;

pub fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
}

#[gpui::test]
async fn test_image_not_loaded_twice(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree("/root", json!({})).await;
    // Create a png file that consists of a single white pixel
    fs.insert_file(
        "/root/image_1.png",
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
    .await;

    let project = Project::test(fs, ["/root".as_ref()], cx).await;

    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());

    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let (task1, task2) = project.update(cx, |project, cx| {
        (
            project.open_image(project_path.clone(), cx),
            project.open_image(project_path.clone(), cx),
        )
    });

    let image1 = task1.await.unwrap();
    let image2 = task2.await.unwrap();

    assert_eq!(image1, image2);
}

#[gpui::test]
fn test_compute_metadata_from_bytes() {
    // Single white pixel PNG
    let png_bytes = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let metadata = ImageItem::compute_metadata_from_bytes(&png_bytes).unwrap();

    assert_eq!(metadata.width, 1);
    assert_eq!(metadata.height, 1);
    assert_eq!(metadata.file_size, png_bytes.len() as u64);
    assert_eq!(metadata.format, image::ImageFormat::Png);
    assert!(metadata.colors.is_some());
}

fn rgb_png(width: u32, height: u32, exif: Option<Vec<u8>>) -> Vec<u8> {
    use image::{ExtendedColorType, ImageEncoder as _, codecs::png::PngEncoder};
    let pixels = vec![0x80u8; (width * height * 3) as usize];
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    if let Some(exif) = exif {
        encoder.set_exif_metadata(exif).unwrap();
    }
    encoder
        .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

#[gpui::test]
fn metadata_of_an_rgb_png_ico_is_read_without_decoding_pixels() {
    // The image crate only decodes an ICO whose PNG entry is RGBA, so a full decode of this file
    // fails; the header of its entry is still readable.
    let entry = rgb_png(32, 32, None);
    let mut ico = vec![0, 0, 1, 0, 1, 0, 32, 32, 0, 0, 1, 0, 24, 0];
    ico.extend((entry.len() as u32).to_le_bytes());
    ico.extend(22u32.to_le_bytes());
    ico.extend(&entry);
    assert!(image::load_from_memory_with_format(&ico, image::ImageFormat::Ico).is_err());

    let metadata = ImageItem::compute_metadata_from_bytes(&ico).unwrap();
    assert_eq!((metadata.width, metadata.height), (32, 32));
    assert_eq!(metadata.format, image::ImageFormat::Ico);
    assert!(metadata.colors.is_some());
}

#[gpui::test]
fn metadata_reports_oriented_dimensions() {
    // A 40 x 20 picture stored landscape with EXIF orientation 6 (rotate 90 degrees) is shown 20 x 40.
    let mut exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    exif.extend(1u16.to_le_bytes());
    exif.extend(0x0112u16.to_le_bytes());
    exif.extend(3u16.to_le_bytes());
    exif.extend(1u32.to_le_bytes());
    exif.extend(6u16.to_le_bytes());
    exif.extend([0, 0]);
    exif.extend(0u32.to_le_bytes());

    let upright = ImageItem::compute_metadata_from_bytes(&rgb_png(40, 20, Some(exif))).unwrap();
    assert_eq!((upright.width, upright.height), (20, 40));
    let plain = ImageItem::compute_metadata_from_bytes(&rgb_png(40, 20, None)).unwrap();
    assert_eq!((plain.width, plain.height), (40, 20));
}

#[gpui::test]
fn metadata_of_an_image_too_large_to_decode_is_still_read() {
    // Only the header of a 16000 x 16000 PNG is present: a pixel decode could not succeed.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(16000u32.to_be_bytes());
    ihdr.extend(16000u32.to_be_bytes());
    ihdr.extend([8, 0, 0, 0, 0]);
    let crc = |kind: &[u8], data: &[u8]| {
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
    };
    png.extend(13u32.to_be_bytes());
    png.extend(b"IHDR");
    png.extend(&ihdr);
    png.extend(crc(b"IHDR", &ihdr).to_be_bytes());
    png.extend(0u32.to_be_bytes());
    png.extend(b"IDAT");
    png.extend(crc(b"IDAT", &[]).to_be_bytes());

    let metadata = ImageItem::compute_metadata_from_bytes(&png).unwrap();
    assert_eq!((metadata.width, metadata.height), (16000, 16000));
}
