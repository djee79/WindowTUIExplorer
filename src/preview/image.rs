//! Decoding a picture and encoding it for the terminal. Blocking: the worker runs this on
//! the blocking pool, never the UI thread.
//!
//! iTerm2 pictures use sentier's own encoder, not ratatui-image's: that one gives the size
//! in pixels, assuming 10×20 px cells, and the terminal can't tell us the real cell size
//! through ConPTY. On a 200 % screen a cell is ~21×43 px, so pictures came out at half size
//! inside a blacked-out box. Giving the size in **cells** lets the terminal scale the
//! picture to exactly the box, whatever the screen scaling. The cell size then only
//! matters for the shape, and 10×20 has the 1:2 shape of every common terminal font.

use image::{DynamicImage, ImageReader};
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;

/// The assumed cell size in pixels (only its shape matters for iTerm2, see above).
const CELL_W: f64 = 10.0;
const CELL_H: f64 = 20.0;
/// iTerm2 pictures are sent at twice that resolution, so they stay sharp on 200 % screens.
const SHARPNESS: f64 = 2.0;

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
    decode(&read(path)?, picker, cols, rows)
}

/// Decodes `bytes` and encodes the picture to fit `cols`×`rows` cells, keeping its shape.
/// Big pictures fill the box; small ones keep their natural size (one cell per 10×20 px)
/// instead of being blown up. CPU only: slow on a huge picture, but it can't hang.
pub fn decode(bytes: &[u8], picker: &Picker, cols: u16, rows: u16) -> Result<ImagePreview, String> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader
        .format()
        .map(|f| format!("{f:?}").to_uppercase())
        .unwrap_or_else(|| "?".to_string());
    let img = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (img.width(), img.height());
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows)?,
        width,
        height,
        format,
        pdf: None,
        thumbnail: false,
    })
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
) -> Result<ImagePreview, String> {
    let width = (f64::from(cols) * CELL_W * SHARPNESS) as u32;
    let rendered =
        crate::platform::pdf_page(bytes, page, width).map_err(|e| format!("PDF: {e}"))?;
    let img = image::load_from_memory(&rendered.png).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows)?,
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
    })
}

/// The Windows thumbnail of the file at `path` (an OS path string), fitted to
/// `cols`×`rows` cells. Windows reads the file itself: the caller times the whole call.
pub fn thumbnail(
    path: &str,
    picker: &Picker,
    cols: u16,
    rows: u16,
) -> Result<ImagePreview, String> {
    let longest = (f64::from(cols) * CELL_W).max(f64::from(rows) * CELL_H) * SHARPNESS;
    // Windows gives an empty message when there's simply no thumbnail handler.
    let th = crate::platform::shell_thumbnail(path, longest as u32)
        .map_err(|_| "no Windows thumbnail".to_string())?;
    let rgba = image::RgbaImage::from_raw(th.width, th.height, th.rgba)
        .ok_or_else(|| "bad thumbnail size".to_string())?;
    let img = DynamicImage::ImageRgba8(rgba);
    Ok(ImagePreview {
        picture: encode(img, picker, cols, rows)?,
        width: th.width,
        height: th.height,
        format: "Windows thumbnail".to_string(),
        pdf: None,
        thumbnail: true,
    })
}

/// A decoded picture, for the terminal: sentier's iTerm2 encoder, or ratatui-image's
/// cells (halfblocks).
fn encode(img: DynamicImage, picker: &Picker, cols: u16, rows: u16) -> Result<Picture, String> {
    if picker.protocol_type() == ProtocolType::Iterm2 {
        let size = fit_cells(img.width(), img.height(), cols, rows);
        return Ok(Picture::Iterm2 {
            data: encode_iterm2(&img, size)?,
            size,
        });
    }
    picker
        .new_protocol(img, Size::new(cols, rows), Resize::Fit(None))
        .map(Picture::Cells)
        .map_err(|e| e.to_string())
}

/// The cells a `w`×`h` px picture covers in a `cols`×`rows` box: as big as fits, keeping
/// its shape, but never bigger than its natural size.
pub fn fit_cells(w: u32, h: u32, cols: u16, rows: u16) -> Size {
    if w == 0 || h == 0 || cols == 0 || rows == 0 {
        return Size::new(0, 0);
    }
    let (w, h) = (f64::from(w), f64::from(h));
    let scale = (f64::from(cols) * CELL_W / w)
        .min(f64::from(rows) * CELL_H / h)
        .min(1.0);
    let c = ((w * scale / CELL_W).round() as u16).clamp(1, cols);
    let r = ((h * scale / CELL_H).round() as u16).clamp(1, rows);
    Size::new(c, r)
}

/// The iTerm2 inline-image sequence for `img` covering `size` cells.
fn encode_iterm2(img: &DynamicImage, size: Size) -> Result<String, String> {
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
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
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
        let Ok(seq) = encode_iterm2(&img, size) else {
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
            let Ok(got) = super::pdf(&pdf, 1, &picker(proto), 40, 30) else {
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
        assert!(super::pdf(b"%PDF-broken", 0, &picker(ProtocolType::Iterm2), 10, 10).is_err());
    }
}
