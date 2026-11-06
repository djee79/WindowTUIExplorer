//! Decoding a picture and encoding it for the terminal. Blocking: the worker runs this on
//! the blocking pool, never the UI thread.

use image::ImageReader;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

/// A picture ready to draw, fitted to the box it was asked for.
pub struct ImagePreview {
    pub protocol: Protocol,
    /// The picture's own size in pixels, and its format (`PNG`, `JPEG`…).
    pub width: u32,
    pub height: u32,
    pub format: String,
}

impl std::fmt::Debug for ImagePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePreview")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// Reads the picture at `path` (an OS path string) and encodes it to fit `cols`×`rows`
/// cells. Smaller pictures keep their size; bigger ones shrink, keeping their shape.
pub fn load(path: &str, picker: &Picker, cols: u16, rows: u16) -> Result<ImagePreview, String> {
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader
        .format()
        .map(|f| format!("{f:?}").to_uppercase())
        .unwrap_or_else(|| "?".to_string());
    let img = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (img.width(), img.height());
    let protocol = picker
        .new_protocol(img, Size::new(cols, rows), Resize::Fit(None))
        .map_err(|e| e.to_string())?;
    Ok(ImagePreview {
        protocol,
        width,
        height,
        format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::picker::ProtocolType;

    fn picker(p: ProtocolType) -> Picker {
        let mut picker = Picker::halfblocks();
        let cell = picker.font_size();
        assert_eq!((cell.width, cell.height), (10, 20));
        picker.set_protocol_type(p);
        picker
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
            let size = got.protocol.size();
            // 400 px wide at 10 px per cell is 40 cells: shrunk to fit 20.
            assert!(size.width <= 20 && size.height <= 20, "{proto:?} {size:?}");
            assert!(size.width > 0 && size.height > 0);
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
}
