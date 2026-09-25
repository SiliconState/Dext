use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, Rgb, RgbImage};
use serde::{Deserialize, Serialize};

pub(crate) const SOURCE_BYTE_CAP: usize = 20 * 1024 * 1024;
pub(crate) const DECODED_PIXEL_CAP: u64 = 40_000_000;
pub(crate) const LONGEST_SIDE_CAP: u32 = 1_568;
pub(crate) const ENCODED_BYTE_CAP: usize = 1_500 * 1024;
pub(crate) const APPROVED_SOURCE_SHA256_FIELD: &str = "_dext_approved_source_sha256";
const BACKGROUND: [u8; 3] = [240, 240, 240];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ImageReference {
    pub(crate) path: String,
    pub(crate) media_type: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_sha256: String,
}

#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub(crate) reference: ImageReference,
    pub(crate) bytes: Vec<u8>,
}

fn png_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 8usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("PNG chunk length is four bytes"),
        ) as usize;
        let kind = &bytes[offset + 4..header_end];
        if kind == b"acTL" {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(4))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
        if kind == b"IEND" {
            return false;
        }
    }
    false
}

fn webp_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 12usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let kind = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(
            bytes[offset + 4..header_end]
                .try_into()
                .expect("WebP chunk length is four bytes"),
        ) as usize;
        if kind == b"ANIM"
            || kind == b"ANMF"
            || kind == b"VP8X"
                && length >= 1
                && bytes.get(header_end).is_some_and(|flags| flags & 0x02 != 0)
        {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(length & 1))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
    }
    false
}

fn sniff_format(bytes: &[u8]) -> Result<ImageFormat, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if png_is_animated(bytes) {
            return Err("animated PNG is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(ImageFormat::Jpeg)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if webp_is_animated(bytes) {
            return Err("animated WebP is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::WebP)
    } else {
        Err("unsupported image format; read_image accepts only PNG, JPEG, and WebP. Convert other images explicitly or use OCR".to_string())
    }
}

fn source_bytes(path: &Path) -> Result<Vec<u8>, String> {
    crate::session::read_regular_file_bytes_with_limit(path, SOURCE_BYTE_CAP, None, "image source")
        .map(|(bytes, _)| bytes)
}

fn decoder_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_image_height = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_alloc = Some(DECODED_PIXEL_CAP.saturating_mul(5));
    limits
}

fn checked_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("image dimensions must be non-zero".to_string());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "image dimensions overflow the decoded-pixel limit".to_string())?;
    if pixels > DECODED_PIXEL_CAP {
        return Err(format!(
            "image has {pixels} decoded pixels, exceeding the {DECODED_PIXEL_CAP} pixel limit"
        ));
    }
    Ok((width, height))
}

fn composite_neutral(image: DynamicImage) -> RgbImage {
    let rgba = image.into_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        rgb.put_pixel(
            x,
            y,
            Rgb([
                ((u16::from(pixel[0]) * alpha + u16::from(BACKGROUND[0]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[1]) * alpha + u16::from(BACKGROUND[1]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[2]) * alpha + u16::from(BACKGROUND[2]) * inverse + 127) / 255)
                    as u8,
            ]),
        );
    }
    rgb
}

fn bounded_jpeg(image: &RgbImage) -> Result<Vec<u8>, String> {
    for quality in [88, 82, 76, 68, 58, 48, 38, 30] {
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|error| format!("could not encode sanitized image: {error}"))?;
        if encoded.len() <= ENCODED_BYTE_CAP {
            return Ok(encoded);
        }
    }
    Err(format!(
        "sanitized image still exceeds the {ENCODED_BYTE_CAP} byte transmission limit"
    ))
}

fn decode_bounded(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("could not initialize bounded image decoder: {error}"))?;
    let orientation = decoder
        .orientation()
        .map_err(|error| format!("could not read image orientation: {error}"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("could not decode image: {error}"))?;
    decoded.apply_orientation(orientation);
    Ok(decoded)
}

fn prepare_bytes(path: PathBuf, bytes: Vec<u8>) -> Result<PreparedImage, String> {
    let source_sha256 = crate::sha256_hex_bytes(&bytes);
    let format = sniff_format(&bytes)?;
    checked_dimensions(&bytes, format)?;
    let decoded = decode_bounded(&bytes, format)?;
    let resized = if decoded.width().max(decoded.height()) > LONGEST_SIDE_CAP {
        decoded.resize(LONGEST_SIDE_CAP, LONGEST_SIDE_CAP, FilterType::Lanczos3)
    } else {
        decoded
    };
    let rgb = composite_neutral(resized);
    let encoded = bounded_jpeg(&rgb)?;
    let path = path
        .to_str()
        .ok_or_else(|| "read_image requires a UTF-8 workspace path".to_string())?
        .to_string();
    Ok(PreparedImage {
        reference: ImageReference {
            path,
            media_type: "image/jpeg".to_string(),
            width: rgb.width(),
            height: rgb.height(),
            source_sha256,
        },
        bytes: encoded,
    })
}

#[cfg(test)]
pub(crate) fn prepare(path: PathBuf) -> Result<PreparedImage, String> {
    let bytes = source_bytes(&path)?;
    prepare_bytes(path, bytes)
}

pub(crate) fn validate_reference(reference: &ImageReference) -> Result<(), String> {
    let path = Path::new(&reference.path);
    if reference.path.is_empty()
        || reference.path.len() > 16 * 1024
        || reference.path.contains('\0')
        || !path.is_absolute()
    {
        return Err("image reference path is invalid".to_string());
    }
    if reference.media_type != "image/jpeg" {
        return Err("image reference media type is invalid".to_string());
    }
    if reference.width == 0
        || reference.height == 0
        || reference.width.max(reference.height) > LONGEST_SIDE_CAP
    {
        return Err("image reference dimensions are invalid".to_string());
    }
    if reference.source_sha256.len() != 64
        || !reference
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("image reference digest is invalid".to_string());
    }
    Ok(())
}

pub(crate) fn prepare_if_unchanged(reference: &ImageReference) -> Result<PreparedImage, String> {
    validate_reference(reference)?;
    let path = PathBuf::from(&reference.path);
    let bytes = source_bytes(&path)?;
    let source_sha256 = crate::sha256_hex_bytes(&bytes);
    if source_sha256 != reference.source_sha256 {
        return Err("source changed since read_image approval".to_string());
    }
    prepare_bytes(path, bytes)
}

fn workspace_path(root: &Path, input: &serde_json::Value) -> Result<PathBuf, String> {
    let raw = input["path"].as_str().ok_or("missing path")?;
    let path = crate::canonical_read_path(root, raw)?;
    // Compare against the canonical root: symlinked ancestors (for example
    // macOS TMPDIR under /var) must match canonical_read_path output.
    let root = crate::session::canonicalize_or_clone(root);
    if !path.starts_with(&root) {
        return Err(format!(
            "read_image only sends images from the active workspace {}; copy the image into the workspace first",
            root.display()
        ));
    }
    Ok(path)
}

pub(crate) fn validate_workspace_path(
    root: &Path,
    input: &serde_json::Value,
) -> Result<(), String> {
    workspace_path(root, input).map(|_| ())
}

pub(crate) fn approval_digest(root: &Path, input: &serde_json::Value) -> Result<String, String> {
    let path = workspace_path(root, input)?;
    source_bytes(&path).map(|bytes| crate::sha256_hex_bytes(&bytes))
}

pub(crate) fn read_tool(root: &Path, input: &serde_json::Value) -> Result<String, String> {
    let expected = input[APPROVED_SOURCE_SHA256_FIELD]
        .as_str()
        .ok_or("missing internal read_image approval digest")?;
    let path = workspace_path(root, input)?;
    let bytes = source_bytes(&path)?;
    if crate::sha256_hex_bytes(&bytes) != expected {
        return Err("source changed after read_image approval".to_string());
    }
    let prepared = prepare_bytes(path, bytes)?;
    serde_json::to_string(&prepared.reference)
        .map_err(|error| format!("could not serialize image reference: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-{label}-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&path).expect("create image test directory");
        path
    }

    fn write_png(path: &Path, width: u32, height: u32, pixel: Rgba<u8>) {
        ImageBuffer::from_pixel(width, height, pixel)
            .save_with_format(path, ImageFormat::Png)
            .expect("write PNG fixture");
    }

    #[test]
    fn png_animation_detection_uses_chunk_types_not_compressed_payload_text() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"IDAT");
        bytes.extend_from_slice(b"acTL");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        assert!(!png_is_animated(&bytes));

        let mut animated = b"\x89PNG\r\n\x1a\n".to_vec();
        animated.extend_from_slice(&8u32.to_be_bytes());
        animated.extend_from_slice(b"acTL");
        animated.extend_from_slice(&[0; 8]);
        animated.extend_from_slice(&0u32.to_be_bytes());
        assert!(png_is_animated(&animated));
    }

    #[test]
    fn rejects_unsupported_and_malformed_inputs() {
        let root = temp_dir("image-invalid");
        let gif = root.join("sample.gif");
        std::fs::write(&gif, b"GIF89a").unwrap();
        assert!(
            prepare(gif)
                .unwrap_err()
                .contains("only PNG, JPEG, and WebP")
        );

        let malformed = root.join("broken.png");
        std::fs::write(&malformed, b"\x89PNG\r\n\x1a\nbroken").unwrap();
        assert!(prepare(malformed).unwrap_err().contains("dimensions"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enforces_source_byte_and_decoded_pixel_limits_before_decode() {
        let root = temp_dir("image-limits");
        let oversized = root.join("oversized.png");
        let file = std::fs::File::create(&oversized).expect("create oversized fixture");
        file.set_len((SOURCE_BYTE_CAP + 1) as u64)
            .expect("size oversized fixture");
        assert!(
            prepare(oversized)
                .unwrap_err()
                .contains("image source 20971520 byte limit")
        );

        let jpeg = root.join("dimensions.jpg");
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .expect("encode JPEG fixture");
        let sof = bytes
            .windows(2)
            .position(|window| matches!(window, [0xff, 0xc0] | [0xff, 0xc2]))
            .expect("JPEG SOF marker");
        bytes[sof + 5..sof + 7].copy_from_slice(&7_000u16.to_be_bytes());
        bytes[sof + 7..sof + 9].copy_from_slice(&7_000u16.to_be_bytes());
        std::fs::write(&jpeg, bytes).expect("write oversized-dimension JPEG");
        assert!(
            prepare(jpeg)
                .unwrap_err()
                .contains("exceeding the 40000000 pixel limit")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounds_dimensions_strips_metadata_and_composites_alpha() {
        let root = temp_dir("image-sanitize");
        let png = root.join("wide.png");
        write_png(&png, 1_600, 2, Rgba([255, 0, 0, 128]));
        let prepared = prepare(png).expect("prepare image");
        assert_eq!(
            (prepared.reference.width, prepared.reference.height),
            (1_568, 2)
        );
        assert_eq!(prepared.reference.media_type, "image/jpeg");
        assert!(prepared.bytes.len() <= ENCODED_BYTE_CAP);
        assert!(
            !prepared
                .bytes
                .windows(6)
                .any(|window| window == b"Exif\0\0")
        );
        let decoded = image::load_from_memory_with_format(&prepared.bytes, ImageFormat::Jpeg)
            .expect("decode sanitized JPEG")
            .to_rgb8();
        let pixel = decoded.get_pixel(0, 0);
        assert!(
            pixel[0] > 220 && pixel[1] > 90 && pixel[2] > 90,
            "{pixel:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_digest_rejects_source_replacement_before_decode() {
        let root = temp_dir("image-approval-change");
        let png = root.join("approval.png");
        write_png(&png, 2, 2, Rgba([0, 0, 0, 255]));
        let mut input = serde_json::json!({"path": png});
        let digest = approval_digest(&root, &input).expect("approval digest");
        write_png(&png, 2, 2, Rgba([255, 255, 255, 255]));
        input[APPROVED_SOURCE_SHA256_FIELD] = serde_json::Value::String(digest);
        assert_eq!(
            read_tool(&root, &input).unwrap_err(),
            "source changed after read_image approval"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn validates_persisted_reference_shape() {
        let valid = ImageReference {
            path: "/workspace/image.png".to_string(),
            media_type: "image/jpeg".to_string(),
            width: 10,
            height: 20,
            source_sha256: "a".repeat(64),
        };
        validate_reference(&valid).expect("valid image reference");

        let mut malformed = valid.clone();
        malformed.path = "relative.png".to_string();
        assert!(validate_reference(&malformed).is_err());
        malformed = valid.clone();
        malformed.media_type = "image/png".to_string();
        assert!(validate_reference(&malformed).is_err());
        malformed = valid.clone();
        malformed.width = LONGEST_SIDE_CAP + 1;
        assert!(validate_reference(&malformed).is_err());
        malformed = valid;
        malformed.source_sha256 = "A".repeat(64);
        assert!(validate_reference(&malformed).is_err());
    }

    #[test]
    fn detects_source_change_before_transmission() {
        let root = temp_dir("image-change");
        let png = root.join("change.png");
        write_png(&png, 2, 2, Rgba([0, 0, 0, 255]));
        let reference = prepare(png.clone()).expect("prepare first image").reference;
        write_png(&png, 2, 2, Rgba([255, 255, 255, 255]));
        assert_eq!(
            prepare_if_unchanged(&reference).unwrap_err(),
            "source changed since read_image approval"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
