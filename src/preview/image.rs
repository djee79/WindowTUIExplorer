//! Decoding a picture and encoding it for the terminal. Blocking: the worker runs this on
//! the blocking pool, never the UI thread.
//!
//! iTerm2 pictures use sentier's own encoder, not ratatui-image's: that one gives the size
//! in pixels, assuming 10×20 px cells, and the terminal can't tell us the real cell size
//! through ConPTY. On a 200 % screen a cell is ~21×43 px, so pictures came out at half size
//! inside a blacked-out box. Giving the size in **cells** lets the terminal scale the
//! picture to exactly the box, whatever the screen scaling. The cell size then only
//! matters for the shape, and 10×20 has the 1:2 shape of every common terminal font.

use super::View;
use image::{DynamicImage, ImageDecoder, ImageReader};
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;

/// The assumed cell size in pixels (only its shape matters for iTerm2, see above).
const CELL_W: f64 = 10.0;
const CELL_H: f64 = 20.0;
/// iTerm2 pictures are sent at twice that resolution, so they stay sharp on 200 % screens.
const SHARPNESS: f64 = 2.0;
/// The widest a zoomed PDF page is rendered, in device-independent pixels.
const MAX_PDF_WIDTH: f64 = 12_000.0;

/// An encoded picture.
pub enum Picture {
    /// An iTerm2 escape sequence covering `size` cells, written after the frame.
    Iterm2 { data: String, size: Size },
    /// Anything ratatui-image draws through the buffer (halfblocks).
    Cells(Protocol),
}

impl Picture {
    /// The cells it covers.
    pub fn size(&self) -> Size {
        match self {
            Picture::Iterm2 { size, .. } => *size,
            Picture::Cells(p) => p.size(),
        }
    }
}

/// A picture ready to draw, fitted to the box it was asked for.
pub struct ImagePreview {
    pub picture: Picture,
    /// The picture's own size in pixels, and its format (`PNG`, `JPEG`…).
    pub width: u32,
    pub height: u32,
    pub format: String,
    /// For a PDF page: which page, of how many, and its size.
    pub pdf: Option<super::PdfInfo>,
    /// A Windows shell thumbnail (its own size says little: no "1280×853 · PNG").
    pub thumbnail: bool,
    /// For a photo: what its EXIF says.
    pub photo: Option<PhotoInfo>,
}

impl std::fmt::Debug for ImagePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePreview")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("cells", &self.picture.size())
            .finish_non_exhaustive()
    }
}

/// Pictures bigger than this on disk aren't read for a preview.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Reads the file at `path` (an OS path string). The only part that can hang (a dead
/// share), so the worker puts its timeout around this alone.
pub fn read(path: &str) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_FILE_BYTES {
        return Err(format!("too big to preview ({} MB)", len / (1024 * 1024)));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

/// [`read`] then [`decode`], for callers without a timeout to place (tests).
#[cfg(test)]
pub fn load(path: &str, picker: &Picker, cols: u16, rows: u16) -> Result<ImagePreview, String> {
    decode(&read(path)?, picker, cols, rows, View::default())
}

/// Zoomed in (full-screen preview): the part of `img` that `view` shows. The crop is
/// then fitted to the box like a whole picture, growing if need be.
fn apply_view(img: DynamicImage, cols: u16, rows: u16, view: View) -> DynamicImage {
    if !view.zoomed() {
        return img;
    }
    let (x, y, w, h) = super::crop_rect(img.width(), img.height(), cols, rows, view);
    img.crop_imm(x, y, w, h)
}

/// Decodes `bytes` and encodes the picture to fit `cols`×`rows` cells, keeping its shape.
/// Big pictures fill the box; small ones keep their natural size (one cell per 10×20 px)
/// instead of being blown up. CPU only: slow on a huge picture, but it can't hang.
pub fn decode(
    bytes: &[u8],
    picker: &Picker,
    cols: u16,
    rows: u16,
    view: View,
) -> Result<ImagePreview, String> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader
        .format()
        .map(|f| format!("{f:?}").to_uppercase())
        .unwrap_or_else(|| "?".to_string());
    // Phones store pictures sideways with an EXIF orientation: turn them upright.
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    let img = apply_view(img, cols, rows, view);
    // A JPEG is a photo: sent as a JPEG again (as a PNG it is ten times the size).
    let photo = format == "JPEG";
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows, view.zoomed(), photo)?,
        width,
        height,
        format,
        pdf: None,
        thumbnail: false,
        photo: photo_info(bytes),
    })
}

/// What a photo's EXIF says (CLAUDE.md section 11: date, camera, GPS yes/no).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhotoInfo {
    /// `2024-06-01 14:32`.
    pub taken: Option<String>,
    /// `Apple iPhone 13`.
    pub camera: Option<String>,
    pub gps: bool,
}

impl PhotoInfo {
    /// `taken 2024-06-01 14:32 · Apple iPhone 13 · GPS`.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = &self.taken {
            parts.push(format!("taken {t}"));
        }
        parts.extend(self.camera.clone());
        if self.gps {
            parts.push("GPS".to_string());
        }
        parts.join(" · ")
    }
}

/// The EXIF of a JPEG / TIFF / HEIF / PNG / WebP, if it has any worth showing.
pub fn photo_info(bytes: &[u8]) -> Option<PhotoInfo> {
    use exif::{In, Tag, Value};
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let text = |tag: Tag| -> Option<String> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Ascii(v) => {
                let s = String::from_utf8_lossy(v.first()?)
                    .trim_matches(['\0', ' '])
                    .to_string();
                (!s.is_empty()).then_some(s)
            }
            _ => None,
        }
    };
    let taken = text(Tag::DateTimeOriginal)
        .or_else(|| text(Tag::DateTime))
        .map(|d| exif_date(&d));
    let camera = camera_name(text(Tag::Make), text(Tag::Model));
    let gps = exif.get_field(Tag::GPSLatitude, In::PRIMARY).is_some();
    let info = PhotoInfo { taken, camera, gps };
    (info != PhotoInfo::default()).then_some(info)
}

/// `2024:06:01 14:32:10` → `2024-06-01 14:32`; anything else as it is.
fn exif_date(d: &str) -> String {
    let b = d.as_bytes();
    if b.len() >= 16 && b[4] == b':' && b[7] == b':' && b[10] == b' ' {
        format!("{}-{}-{} {}", &d[..4], &d[5..7], &d[8..10], &d[11..16])
    } else {
        d.to_string()
    }
}

/// `Canon` + `Canon EOS 80D` → `Canon EOS 80D`; `Apple` + `iPhone 13` → `Apple iPhone 13`.
fn camera_name(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            let first = make.split_whitespace().next().unwrap_or_default();
            if model.to_lowercase().starts_with(&first.to_lowercase()) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (make, model) => model.or(make),
    }
}

/// Renders page `page` (0-based, clamped) of the PDF in `bytes` and encodes it to fit
/// `cols`×`rows` cells. Windows renders it (`platform::pdf_page`), sharp enough for the
/// box at twice the cell resolution.
pub fn pdf(
    bytes: &[u8],
    page: u32,
    picker: &Picker,
    cols: u16,
    rows: u16,
    view: View,
) -> Result<ImagePreview, String> {
    // Zoomed in, the page is rendered bigger, so the crop stays sharp.
    let width = (f64::from(cols) * CELL_W * SHARPNESS * f64::from(view.zoom) / 100.0)
        .min(MAX_PDF_WIDTH) as u32;
    let rendered =
        crate::platform::pdf_page(bytes, page, width).map_err(|e| format!("PDF: {e}"))?;
    let img = image::load_from_memory(&rendered.png).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let img = apply_view(img, cols, rows, view);
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows, view.zoomed(), false)?,
        width: w,
        height: h,
        format: "PDF".to_string(),
        pdf: Some(super::PdfInfo::from_dip(
            rendered.index,
            rendered.count,
            rendered.width_dip,
            rendered.height_dip,
        )),
        thumbnail: false,
        photo: None,
    })
}

/// The Windows thumbnail of the file at `path` (an OS path string), fitted to
/// `cols`×`rows` cells. Windows reads the file itself: the caller times the whole call.
pub fn thumbnail(
    path: &str,
    picker: &Picker,
    cols: u16,
    rows: u16,
    view: View,
) -> Result<ImagePreview, String> {
    let longest =
        (f64::from(cols) * CELL_W).max(f64::from(rows) * CELL_H) * SHARPNESS * f64::from(view.zoom)
            / 100.0;
    // Windows gives an empty message when there's simply no thumbnail handler.
    let th = crate::platform::shell_thumbnail(path, longest as u32)
        .map_err(|_| "no Windows thumbnail".to_string())?;
    let rgba = image::RgbaImage::from_raw(th.width, th.height, th.rgba)
        .ok_or_else(|| "bad thumbnail size".to_string())?;
    let img = apply_view(DynamicImage::ImageRgba8(rgba), cols, rows, view);
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows, view.zoomed(), false)?,
        width: th.width,
        height: th.height,
        format: "Windows thumbnail".to_string(),
        pdf: None,
        thumbnail: true,
        photo: None,
    })
}

/// A decoded picture, for the terminal: sentier's iTerm2 encoder, or ratatui-image's
/// cells (halfblocks). `grow`: fill the box even past the natural size (a zoomed crop).
/// `photo`: it came from a JPEG, so iTerm2 gets a JPEG.
fn encode(
    img: DynamicImage,
    picker: &Picker,
    cols: u16,
    rows: u16,
    grow: bool,
    photo: bool,
) -> Result<Picture, String> {
    if picker.protocol_type() == ProtocolType::Iterm2 {
        let size = fit(img.width(), img.height(), cols, rows, grow);
        return Ok(Picture::Iterm2 {
            data: encode_iterm2(&img, size, photo)?,
            size,
        });
    }
    let resize = if grow {
        Resize::Scale(None)
    } else {
        Resize::Fit(None)
    };
    picker
        .new_protocol(img, Size::new(cols, rows), resize)
        .map(Picture::Cells)
        .map_err(|e| e.to_string())
}

/// The cells a `w`×`h` px picture covers in a `cols`×`rows` box: as big as fits, keeping
/// its shape, but never bigger than its natural size.
#[cfg(test)]
pub fn fit_cells(w: u32, h: u32, cols: u16, rows: u16) -> Size {
    fit(w, h, cols, rows, false)
}

/// [`fit_cells`], optionally growing past the natural size.
fn fit(w: u32, h: u32, cols: u16, rows: u16, grow: bool) -> Size {
    if w == 0 || h == 0 || cols == 0 || rows == 0 {
        return Size::new(0, 0);
    }
    let (w, h) = (f64::from(w), f64::from(h));
    let scale = (f64::from(cols) * CELL_W / w).min(f64::from(rows) * CELL_H / h);
    let scale = if grow { scale } else { scale.min(1.0) };
    let c = ((w * scale / CELL_W).round() as u16).clamp(1, cols);
    let r = ((h * scale / CELL_H).round() as u16).clamp(1, rows);
    Size::new(c, r)
}

/// JPEG quality for photos: no visible loss at preview size.
const PHOTO_QUALITY: u8 = 85;

/// The bytes sent to the terminal for `img`. The fewer, the sooner the picture is up:
/// they are written from the UI loop and the terminal has to take them all in.
/// - A photo (from a JPEG) goes as a JPEG: about a tenth of the same picture as a PNG.
/// - Everything else (screenshots, drawings, PDF pages) as a PNG, exact. Deflate's
///   default level, not the encoder's "fast" one: measured on screenshots at preview
///   size, the result is 45 % smaller and no slower to make (2026-11-11).
fn picture_bytes(img: &DynamicImage, photo: bool) -> Result<Vec<u8>, String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let mut out = Vec::new();
    if photo {
        img.to_rgb8()
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, PHOTO_QUALITY))
    } else {
        img.write_with_encoder(PngEncoder::new_with_quality(
            &mut out,
            CompressionType::Default,
            FilterType::Adaptive,
        ))
    }
    .map_err(|e| e.to_string())?;
    Ok(out)
}

/// The iTerm2 inline-image sequence for `img` covering `size` cells.
fn encode_iterm2(img: &DynamicImage, size: Size, photo: bool) -> Result<String, String> {
    // Pixels worth sending: the box at double resolution, at most the original.
    let max_w = (f64::from(size.width) * CELL_W * SHARPNESS) as u32;
    let max_h = (f64::from(size.height) * CELL_H * SHARPNESS) as u32;
    let img = if img.width() > max_w || img.height() > max_h {
        // `thumbnail` is built for big reductions: much faster than a filtered resize,
        // and the terminal scales the result again anyway.
        img.thumbnail(max_w.max(1), max_h.max(1))
    } else {
        img.clone()
    };
    let png = picture_bytes(&img, photo)?;
    let mut seq = format!(
        "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:",
        png.len(),
        size.width,
        size.height
    );
    base64_into(&png, &mut seq);
    seq.push('\x07');
    Ok(seq)
}

/// Standard base64 with padding, appended to `out`.
fn base64_into(bytes: &[u8], out: &mut String) {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let digits = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, d) in digits.iter().enumerate() {
            if i <= chunk.len() {
                out.push(ABC[*d as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(p: ProtocolType) -> Picker {
        let mut picker = Picker::halfblocks();
        let cell = picker.font_size();
        assert_eq!((cell.width, cell.height), (10, 20));
        picker.set_protocol_type(p);
        picker
    }

    #[test]
    fn base64_table() {
        // RFC 4648 test vectors.
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            let mut s = String::new();
            base64_into(input.as_bytes(), &mut s);
            assert_eq!(&s, want, "{input}");
        }
        let mut s = String::new();
        base64_into(&[0xff, 0xfe, 0x00], &mut s);
        assert_eq!(s, "//4A");
    }

    /// (picture w, h, box cols, rows) → (cols, rows) covered.
    type FitCase = ((u32, u32, u16, u16), (u16, u16));

    #[test]
    fn fit_table() {
        let cases: &[FitCase] = &[
            // The cheatsheet (3311×2851) in a 48×57 box: width-bound, fills the width.
            ((3311, 2851, 48, 57), (48, 21)),
            // A portrait page (1700×2200) in the same box: width-bound too, 31 rows tall.
            ((1700, 2200, 48, 57), (48, 31)),
            // The same page in a short box: height-bound, 20 rows, 31 cols.
            ((1700, 2200, 48, 20), (31, 20)),
            // A 32 px icon keeps its natural size.
            ((32, 32, 48, 57), (3, 2)),
            // Never zero, never out of the box.
            ((1, 1, 48, 57), (1, 1)),
            ((5000, 10, 48, 57), (48, 1)),
            ((0, 10, 48, 57), (0, 0)),
            ((100, 100, 0, 57), (0, 0)),
        ];
        for ((w, h, c, r), (wc, wr)) in cases {
            let got = fit_cells(*w, *h, *c, *r);
            assert_eq!((got.width, got.height), (*wc, *wr), "{w}×{h} in {c}×{r}");
        }
    }

    #[test]
    fn iterm2_sequence_is_sized_in_cells() {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1655,
            1425,
            image::Rgba([1, 2, 3, 255]),
        ));
        let size = fit_cells(1655, 1425, 48, 57);
        let Ok(seq) = encode_iterm2(&img, size, false) else {
            panic!()
        };
        assert!(
            seq.starts_with("\x1b]1337;File=inline=1;size="),
            "{}",
            &seq[..40]
        );
        assert!(seq.contains(";width=48;height=21;preserveAspectRatio=1;"));
        assert!(seq.ends_with('\x07'));
        assert!(!seq.contains("\x1b["), "no cursor moves or erases");
    }

    #[test]
    fn photos_go_as_jpeg_the_rest_as_exact_png() {
        // Noise, like a photo: PNG can't shrink it, JPEG can.
        let mut seed = 7u32;
        let noisy = image::RgbImage::from_fn(320, 240, |x, y| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (seed >> 24) as u8 / 8;
            image::Rgb([(x / 2) as u8 + n, (y / 2) as u8 + n, 90 + n])
        });
        let img = DynamicImage::ImageRgb8(noisy);
        let (Ok(jpeg), Ok(png)) = (picture_bytes(&img, true), picture_bytes(&img, false)) else {
            panic!()
        };
        assert!(jpeg.starts_with(&[0xff, 0xd8]), "a JPEG");
        assert!(png.starts_with(b"\x89PNG"), "a PNG");
        assert!(
            jpeg.len() * 3 < png.len(),
            "{} vs {}",
            jpeg.len(),
            png.len()
        );
        // The PNG is exact.
        let Ok(back) = image::load_from_memory(&png) else {
            panic!()
        };
        assert_eq!(back.to_rgb8(), img.to_rgb8());
        // A picture with see-through parts keeps them.
        let clear = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([1, 2, 3, 0]),
        ));
        let Ok(png) = picture_bytes(&clear, false) else {
            panic!()
        };
        assert!(image::load_from_memory(&png).is_ok_and(|i| i.color().has_alpha()));
    }

    #[test]
    fn loads_and_fits_a_png() {
        let dir = std::env::temp_dir().join(format!("sentier-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("card.png");
        let img = image::RgbaImage::from_pixel(400, 200, image::Rgba([10, 200, 30, 255]));
        assert!(img.save(&path).is_ok());
        let p = path.to_string_lossy().to_string();

        for proto in [ProtocolType::Halfblocks, ProtocolType::Iterm2] {
            let Ok(got) = load(&p, &picker(proto), 20, 20) else {
                panic!("{proto:?}")
            };
            assert_eq!(
                (got.width, got.height, got.format.as_str()),
                (400, 200, "PNG")
            );
            let size = got.picture.size();
            // 400 px wide at 10 px per cell is 40 cells: shrunk to fit 20.
            assert!(size.width <= 20 && size.height <= 20, "{proto:?} {size:?}");
            assert!(size.width > 0 && size.height > 0);
            let is_iterm2 = matches!(got.picture, Picture::Iterm2 { .. });
            assert_eq!(is_iterm2, proto == ProtocolType::Iterm2);
        }
        assert!(
            load(
                &dir.join("nope.png").to_string_lossy(),
                &picker(ProtocolType::Halfblocks),
                10,
                10
            )
            .is_err()
        );
        let txt = dir.join("not-an-image.png");
        let _ = std::fs::write(&txt, b"hello");
        assert!(
            load(
                &txt.to_string_lossy(),
                &picker(ProtocolType::Halfblocks),
                10,
                10
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pdf_pages_render_and_report_their_size() {
        let pdf = crate::platform::tests::tiny_pdf(&[(595, 842), (1191, 842)]);
        for proto in [ProtocolType::Halfblocks, ProtocolType::Iterm2] {
            let Ok(got) = super::pdf(&pdf, 1, &picker(proto), 40, 30, View::default()) else {
                panic!("{proto:?}")
            };
            let Some(info) = &got.pdf else { panic!() };
            assert_eq!((info.page, info.count), (1, 2));
            assert!(
                info.caption().contains("A3 landscape"),
                "{}",
                info.caption()
            );
            let size = got.picture.size();
            assert!(size.width <= 40 && size.height <= 30 && size.width > 0);
            // Landscape: wider than tall, in cells of 1:2.
            assert!(size.width > size.height, "{size:?}");
        }
        assert!(
            super::pdf(
                b"%PDF-broken",
                0,
                &picker(ProtocolType::Iterm2),
                10,
                10,
                View::default()
            )
            .is_err()
        );
    }

    #[test]
    fn zoomed_crops_grow_to_fill_the_box() {
        let mut png = Vec::new();
        assert!(
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                100,
                100,
                image::Rgba([1, 2, 3, 255])
            ))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .is_ok()
        );
        let p = picker(ProtocolType::Iterm2);
        // 100×100 px is 10×5 cells naturally; the box is 40×20.
        let Ok(fit) = decode(&png, &p, 40, 20, View::default()) else {
            panic!()
        };
        assert_eq!(
            fit.picture.size(),
            Size::new(10, 5),
            "natural size at 100 %"
        );
        let zoomed = View::default().step_zoom(true).step_zoom(true);
        let Ok(z) = decode(&png, &p, 40, 20, zoomed) else {
            panic!()
        };
        let s = z.picture.size();
        assert!(s.height == 20 || s.width == 40, "fills the box: {s:?}");
        assert_eq!((z.width, z.height), (100, 100), "the picture's own size");
    }

    #[test]
    fn exif_dates_and_camera_names() {
        assert_eq!(exif_date("2024:06:01 14:32:10"), "2024-06-01 14:32");
        assert_eq!(exif_date("garbage"), "garbage");
        let s = |v: &str| Some(v.to_string());
        let cases = [
            (s("Canon"), s("Canon EOS 80D"), s("Canon EOS 80D")),
            (s("Apple"), s("iPhone 13"), s("Apple iPhone 13")),
            (s("NIKON CORPORATION"), s("NIKON D750"), s("NIKON D750")),
            (None, s("X100V"), s("X100V")),
            (s("Sony"), None, s("Sony")),
            (None, None, None),
        ];
        for (make, model, want) in cases {
            assert_eq!(
                camera_name(make.clone(), model.clone()),
                want,
                "{make:?} {model:?}"
            );
        }
        let info = PhotoInfo {
            taken: s("2024-06-01 14:32"),
            camera: s("Apple iPhone 13"),
            gps: true,
        };
        assert_eq!(
            info.line(),
            "taken 2024-06-01 14:32 · Apple iPhone 13 · GPS"
        );
    }

    #[test]
    fn reads_exif_from_the_file_bytes() {
        use exif::{Field, In, Rational, Tag, Value};
        let fields = [
            Field {
                tag: Tag::Make,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"Apple".to_vec()]),
            },
            Field {
                tag: Tag::Model,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"iPhone 13".to_vec()]),
            },
            Field {
                tag: Tag::DateTimeOriginal,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![b"2024:06:01 14:32:10".to_vec()]),
            },
            Field {
                tag: Tag::GPSLatitude,
                ifd_num: In::PRIMARY,
                value: Value::Rational(vec![
                    Rational::from((45, 1)),
                    Rational::from((30, 1)),
                    Rational::from((0, 1)),
                ]),
            },
        ];
        let mut writer = exif::experimental::Writer::new();
        for f in &fields {
            writer.push_field(f);
        }
        let mut tiff = std::io::Cursor::new(Vec::new());
        assert!(writer.write(&mut tiff, true).is_ok());
        let info = photo_info(tiff.get_ref());
        assert_eq!(
            info.map(|i| i.line()).as_deref(),
            Some("taken 2024-06-01 14:32 · Apple iPhone 13 · GPS")
        );
        assert_eq!(photo_info(b"not an image"), None);
    }
}
