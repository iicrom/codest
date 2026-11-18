//! Image loading for the `read_image` tool, distilled from codex's
//! `utils/image` crate.
//!
//! The model never receives the file's bytes: a screenshot straight off a
//! retina display is a couple of megapixels and would blow through the
//! prompt's image budget for no extra legibility. Codex's rule is a **patch
//! budget** (32 px patches): `high` allows 2500 patches in at most 2048 px
//! per side — roughly a 2048x1536 image — which is where a downscaled
//! screenshot still reads fine. `original` lifts that to 10 000 patches and
//! 6000 px and is only offered to models that ask for it (see
//! [`ToolSpec`](cst_plugin_api::tools::ToolSpec) `detail` in `fs.rs`).
//!
//! Scaling preserves aspect ratio: first shrink so the longest side fits the
//! budget's [`HIGH_DETAIL_MAX_DIMENSION`] (or its `original` counterpart),
//! then, if the patch count is still over budget, shrink by area and round
//! the resulting patch grid *down*, so the integer output dimensions stay
//! inside the budget. The encoder keeps the source format when it is one that
//! can be passed through (PNG/JPEG/WebP); a re-encode (a GIF frame, say)
//! always lands as PNG.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::ImageFormat;
use image::ImageReader;
use image::imageops::FilterType;

use cst_plugin_api::tools::ToolImage;

/// Width/height of one patch in the model's image token accounting.
const PATCH_SIZE: u32 = 32;
/// Longest side (and per-side cap) of a `high`-detail image.
const HIGH_DETAIL_MAX_DIMENSION: u32 = 2048;
/// Patch budget of a `high`-detail image.
const HIGH_DETAIL_MAX_PATCHES: usize = 2_500;
/// Longest side (and per-side cap) of an `original`-detail image.
const ORIGINAL_DETAIL_MAX_DIMENSION: u32 = 6_000;
/// Patch budget of an `original`-detail image.
const ORIGINAL_DETAIL_MAX_PATCHES: usize = 10_000;
/// Hard cap on the bytes read before decoding. Well above any screenshot a
/// tool should be handed, low enough that a mislabeled multi-GB file is
/// refused without being buffered first.
const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
/// Hard cap on either side the decoder will materialize. The `original`
/// budget resizes *down* to 6000 px, so anything beyond this is a file
/// nothing would display anyway — and refusing it at the decoder keeps a
/// decompression bomb from allocating a canvas the tool would only shrink.
const MAX_DECODE_DIMENSION: u32 = 16_384;

/// What the caller asked for: which budget to apply and what `detail` the
/// request should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    High,
    Original,
}

impl Detail {
    /// The wire value of `detail`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Detail::High => "high",
            Detail::Original => "original",
        }
    }

    fn limits(self) -> (u32, usize) {
        match self {
            Detail::High => (HIGH_DETAIL_MAX_DIMENSION, HIGH_DETAIL_MAX_PATCHES),
            Detail::Original => (ORIGINAL_DETAIL_MAX_DIMENSION, ORIGINAL_DETAIL_MAX_PATCHES),
        }
    }
}

/// A prepared image plus what it took to get there, for the result line.
pub(crate) struct Prepared {
    /// The image as it goes on the wire, ready to attach to the tool result.
    pub(crate) image: ToolImage,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The bytes actually attached, after any re-encode.
    pub(crate) encoded_bytes: usize,
}

impl Prepared {
    /// `2304x864 → 2048x768, image/png, 412 KB` — the shape the result line
    /// reports, so the model knows the image it sees is not the original.
    pub(crate) fn describe(&self) -> String {
        let size = if self.source_width == self.width && self.source_height == self.height {
            format!("{}x{}", self.width, self.height)
        } else {
            format!(
                "{}x{} → {}x{}",
                self.source_width, self.source_height, self.width, self.height
            )
        };
        format!(
            "{size}, {}, {}",
            self.image.mime,
            human_bytes(self.encoded_bytes)
        )
    }
}

/// `412 KB`, `1.2 MB` — a byte count a result line can carry.
pub(crate) fn human_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Read and prepare the image at `path`.
///
/// `Err` is the message the model gets: a distinct one per failure (not a
/// file, unreadable, too large, not an image), because the useful next step
/// differs — `read_image` on a directory is a wrong tool, while a decode
/// failure usually means a stale path or a truncated file.
pub(crate) fn prepare(path: &std::path::Path, detail: Detail) -> Result<Prepared, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("file not found: {}", path.display())
        } else {
            format!("cannot stat `{}`: {error}", path.display())
        }
    })?;
    if !metadata.is_file() {
        return Err(format!("`{}` is not a file", path.display()));
    }
    if metadata.len() as usize > MAX_INPUT_BYTES {
        return Err(format!(
            "`{}` is {}; the limit is {}",
            path.display(),
            human_bytes(metadata.len() as usize),
            human_bytes(MAX_INPUT_BYTES)
        ));
    }

    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
    prepare_bytes(&bytes, detail)
}

/// [`prepare`] over bytes already in memory — the seam tests use, and what a
/// future screenshot/attachment source would call.
pub(crate) fn prepare_bytes(bytes: &[u8], detail: Detail) -> Result<Prepared, String> {
    let format = image::guess_format(bytes).map_err(|_| {
        "not a recognized image (supported: PNG, JPEG, GIF, WebP); use `read` for text files"
            .to_string()
    })?;
    let format = match format {
        ImageFormat::Png => ImageFormat::Png,
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        ImageFormat::Gif => ImageFormat::Gif,
        ImageFormat::WebP => ImageFormat::WebP,
        other => {
            return Err(format!(
                "unsupported image format {other:?} (supported: PNG, JPEG, GIF, WebP)"
            ));
        }
    };

    // The decode itself is bounded: a small file can still declare a huge
    // canvas, and without dimension caps the decoder would allocate it in
    // full before [`output_dimensions`] ever runs. The cap is well above
    // anything a screenshot is, so it only ever catches a decompression bomb.
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| format!("cannot decode the image: {error}"))?;
    let (source_width, source_height) = (decoded.width(), decoded.height());
    if source_width == 0 || source_height == 0 {
        return Err("the image has no pixels".to_string());
    }

    let (max_dimension, max_patches) = detail.limits();
    let (width, height) =
        output_dimensions(source_width, source_height, max_dimension, max_patches);

    // Attach the source bytes untouched when the size already fits and the
    // format can be passed through: re-encoding a PNG screenshot would only
    // lose time and bytes. GIF is re-encoded (its first frame) and any
    // resize re-encodes, both to a format the backend reads.
    let (encoded, mime) = if (width, height) == (source_width, source_height)
        && matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
        ) {
        (bytes.to_vec(), mime_of(format).to_string())
    } else {
        let resized = decoded.resize_exact(width, height, FilterType::Triangle);
        let target = if matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
        ) {
            format
        } else {
            ImageFormat::Png
        };
        let mut buffer = Vec::new();
        resized
            .write_to(&mut Cursor::new(&mut buffer), target)
            .map_err(|error| format!("cannot encode the image: {error}"))?;
        (buffer, mime_of(target).to_string())
    };

    let encoded_bytes = encoded.len();
    let data_url = format!("data:{mime};base64,{}", BASE64_STANDARD.encode(&encoded));
    Ok(Prepared {
        image: ToolImage {
            data_url,
            mime,
            detail: detail.as_str().to_string(),
        },
        source_width,
        source_height,
        width,
        height,
        encoded_bytes,
    })
}

/// The dimensions the model gets, from the source's: fit the longest side
/// under `max_dimension`, then — if the patch grid is still too large —
/// shrink by area and round the grid down so the integer output stays inside
/// the budget (codex's `prompt_image_output_dimensions_for_limits`).
fn output_dimensions(
    width: u32,
    height: u32,
    max_dimension: u32,
    max_patches: usize,
) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    if dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let max_dimension_scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let patch_size = f64::from(PATCH_SIZE);
    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let mut scale = (patch_size * patch_size * max_patches as f64 / width_f64 / height_f64).sqrt();
    let scaled_wide = width_f64 * scale / patch_size;
    let scaled_high = height_f64 * scale / patch_size;
    scale *= (scaled_wide.floor() / scaled_wide).min(scaled_high.floor() / scaled_high);
    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn dimensions_fit(width: u32, height: u32, max_dimension: u32, max_patches: usize) -> bool {
    let patches_wide = width.div_ceil(PATCH_SIZE);
    let patches_high = height.div_ceil(PATCH_SIZE);
    let patch_count = u64::from(patches_wide) * u64::from(patches_high);
    width <= max_dimension && height <= max_dimension && patch_count <= max_patches as u64
}

fn mime_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

/// Whether the first bytes of a file carry one of the four signature headers
/// [`prepare_bytes`] accepts. `read` uses it to point a binary file at
/// `read_image` instead of the shell when it is recognizably an image.
pub(crate) fn looks_like_an_image(head: &[u8]) -> bool {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const GIF87: &[u8] = b"GIF87a";
    const GIF89: &[u8] = b"GIF89a";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    head.starts_with(PNG)
        || head.starts_with(GIF87)
        || head.starts_with(GIF89)
        || head.starts_with(JPEG)
        // RIFF....WEBP — the four-byte size sits between the two tags.
        || (head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use image::RgbaImage;

    /// A solid-color image of the given size.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200u8, 40, 90, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode");
        bytes
    }

    fn decoded_size(prepared: &Prepared) -> (u32, u32) {
        let payload = prepared
            .image
            .data_url
            .split_once(',')
            .expect("a data URL")
            .1;
        let bytes = BASE64_STANDARD.decode(payload).expect("base64");
        let decoded = image::load_from_memory(&bytes).expect("decode prepared");
        (decoded.width(), decoded.height())
    }

    #[test]
    fn a_small_image_is_attached_untouched() {
        let source = png(64, 48);
        let prepared = prepare_bytes(&source, Detail::High).expect("prepare");
        assert_eq!((prepared.width, prepared.height), (64, 48));
        assert_eq!(prepared.image.mime, "image/png");
        assert_eq!(prepared.image.detail, "high");
        assert!(
            prepared
                .image
                .data_url
                .starts_with("data:image/png;base64,")
        );
        // Passed through byte for byte, not re-encoded.
        assert_eq!(decoded_size(&prepared), (64, 48));
        assert!(prepared.describe().contains("64x48"));
        assert!(!prepared.describe().contains('→'));
    }

    /// A screenshot-sized image is scaled to the `high` budget: the longest
    /// side lands at 2048 and the patch grid is inside 2500.
    #[test]
    fn a_large_image_is_scaled_to_the_high_budget() {
        let prepared = prepare_bytes(&png(2304, 864), Detail::High).expect("prepare");
        assert_eq!(prepared.width, 2048);
        assert_eq!(prepared.height, 768);
        assert_eq!(decoded_size(&prepared), (2048, 768));
        assert!(prepared.describe().contains("2304x864 → 2048x768"));
    }

    /// Aspect ratio survives the resize: a tall image keeps its proportions
    /// within a pixel of rounding.
    #[test]
    fn the_aspect_ratio_survives_the_budget() {
        let prepared = prepare_bytes(&png(1000, 3000), Detail::High).expect("prepare");
        assert!(prepared.height <= HIGH_DETAIL_MAX_DIMENSION);
        assert!(prepared.width <= HIGH_DETAIL_MAX_DIMENSION);
        let source_ratio = 1000.0 / 3000.0;
        let prepared_ratio = prepared.width as f64 / prepared.height as f64;
        assert!(
            (source_ratio - prepared_ratio).abs() < 0.01,
            "{prepared_ratio}"
        );
    }

    /// The patch budget binds before the dimension cap for a near-square
    /// image: 2048x2048 is 64x64 patches (4096) — over 2500 — so the area
    /// scale trims it further.
    #[test]
    fn the_patch_budget_binds_before_the_dimension_cap() {
        let prepared = prepare_bytes(&png(2048, 2048), Detail::High).expect("prepare");
        let patches = prepared.width.div_ceil(PATCH_SIZE) * prepared.height.div_ceil(PATCH_SIZE);
        assert!(
            patches <= HIGH_DETAIL_MAX_PATCHES as u32,
            "{patches} patches"
        );
        assert!(prepared.width < 2048);
    }

    /// `original` keeps more resolution than `high` for the same source.
    #[test]
    fn original_keeps_more_detail_than_high() {
        let source = png(4000, 2000);
        let high = prepare_bytes(&source, Detail::High).expect("high");
        let original = prepare_bytes(&source, Detail::Original).expect("original");
        assert!(original.width > high.width);
        assert_eq!(original.image.detail, "original");
        assert!(original.width <= ORIGINAL_DETAIL_MAX_DIMENSION);
    }

    /// A GIF is re-encoded to PNG — codex passes only PNG/JPEG/WebP bytes
    /// through, because an animated GIF's later frames do not survive the
    /// plain image decode.
    #[test]
    fn a_gif_is_re_encoded_as_png() {
        let source = {
            let frame = RgbaImage::from_pixel(40, 30, Rgba([10u8, 200, 30, 255]));
            let mut bytes = Vec::new();
            image::codecs::gif::GifEncoder::new(&mut bytes)
                .encode_frame(image::Frame::new(frame))
                .expect("gif encode");
            bytes
        };
        let prepared = prepare_bytes(&source, Detail::High).expect("prepare");
        assert_eq!(prepared.image.mime, "image/png");
        assert_eq!(decoded_size(&prepared), (40, 30));
    }

    #[test]
    fn non_images_are_refused_with_a_pointer_to_read() {
        let error = prepare_bytes(b"plain text, not an image", Detail::High)
            .err()
            .expect("an error");
        assert!(error.contains("not a recognized image"), "{error}");
        assert!(error.contains("`read`"), "{error}");
    }

    #[test]
    fn a_missing_file_is_reported_as_missing() {
        let error = prepare(
            std::path::Path::new("/nonexistent/codest/read-image-test.png"),
            Detail::High,
        )
        .err()
        .expect("an error");
        assert!(error.contains("file not found"), "{error}");
    }

    #[test]
    fn human_bytes_uses_readable_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(4096), "4 KB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    /// The reported size is the image's own bytes, not the 4/3 base64 the
    /// wire carries — the result line and the `/context` inspector both show
    /// the file's cost, so they must agree with each other and with the file.
    #[test]
    fn the_attachment_reports_the_encoded_file_size() {
        let prepared = prepare_bytes(&png(64, 48), Detail::High).expect("prepare");
        // The payload is base64 (4 encoded chars per 3 bytes), so decoding it
        // recovers exactly the bytes the encoder wrote.
        assert_eq!(prepared.image.decoded_bytes(), prepared.encoded_bytes);
        assert!(prepared.image.payload_bytes() > prepared.image.decoded_bytes());
        let payload = prepared.image.data_url.split_once(',').expect("a data URL").1;
        let raw = BASE64_STANDARD.decode(payload).expect("base64");
        assert_eq!(raw.len(), prepared.image.decoded_bytes());
    }

    /// A file that declares a canvas past the decode cap is refused at the
    /// decoder, before anything is allocated for it. These 68 bytes are a
    /// real PNG whose IHDR claims 20000x20000 — the shape of a decompression
    /// bomb, where the pixel data costs almost nothing but the canvas does
    /// not. Sizing it down afterwards would already be too late.
    #[test]
    fn a_bomb_sized_canvas_is_refused_at_the_decoder() {
        const BOMB: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x4e, 0x20, 0x00, 0x00, 0x4e, 0x20, 0x08, 0x06, 0x00, 0x00,
            0x00, 0xe3, 0x70, 0x46, 0x39, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let error = prepare_bytes(BOMB, Detail::High)
            .err()
            .expect("a 20000x20000 canvas must be refused");
        assert!(error.contains("cannot decode"), "{error}");
        // The cap only ever catches a bomb: everything the tool would keep,
        // `original`'s 6000 px included, still gets through.
        assert!(prepare_bytes(&png(7000, 100), Detail::Original).is_ok());
    }

    /// `read` points at `read_image` only for bytes that are recognizably an
    /// image; anything else stays on the shell hint.
    #[test]
    fn image_signatures_are_recognized() {
        assert!(looks_like_an_image(&png(2, 2)));
        assert!(looks_like_an_image(b"GIF89a...."));
        assert!(looks_like_an_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_an_image(b"RIFF\x10\x00\x00\x00WEBPdata"));
        assert!(!looks_like_an_image(b"RIFF\x10\x00\x00\x00WAVEfmt "));
        assert!(!looks_like_an_image(b"plain text"));
        assert!(!looks_like_an_image(&[0x89, b'P', 0, b'N', 0, b'G']));
    }
}
