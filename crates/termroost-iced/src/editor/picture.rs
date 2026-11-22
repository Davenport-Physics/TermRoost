//! Pictures opened from the Files tab. PNG, JPEG, GIF, WebP and the other
//! raster formats are decoded here, on a thread, so the window doesn't stall
//! and a broken file says why; an SVG is only checked and measured here, and
//! iced draws it at whatever size it is shown.

use std::io::Cursor;
use std::path::Path;

use iced::Vector;
use iced::advanced::image as raster;
use iced::advanced::svg;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader};

use super::disk::{self, Stamp};

/// Larger files are not opened.
const MAX_SIZE: u64 = 64 * 1024 * 1024;

/// Extensions (lowercase) of the raster pictures that can be shown.
const RASTER: &[&str] = &[
    "png", "jpg", "jpeg", "jfif", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi", "pnm", "pbm", "pgm", "ppm",
    "pam",
];
/// Extensions of vector pictures; `svgz` is a gzipped SVG.
const VECTOR: &[&str] = &["svg", "svgz"];

/// What kind of picture a file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raster,
    Vector,
}

/// The kind of picture the file at `path` is, by its extension; `None` for
/// any other file. Case doesn't matter.
pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VECTOR.contains(&ext.as_str()) {
        Some(Kind::Vector)
    } else if RASTER.contains(&ext.as_str()) {
        Some(Kind::Raster)
    } else {
        None
    }
}

/// The picture at `path` is text that can be edited: an SVG, not gzipped.
pub fn has_text(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"))
}

/// A picture ready to draw.
#[derive(Clone)]
pub struct Picture {
    pub image: Image,
    /// Its size in pixels; an SVG's own size.
    pub width: u32,
    pub height: u32,
    /// What it is stored as, such as "PNG".
    pub format: &'static str,
    /// Parts of it can be seen through, so a checkerboard goes under it.
    pub transparent: bool,
}

#[derive(Clone)]
pub enum Image {
    Raster(raster::Handle),
    Vector(svg::Handle),
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the pixels.
        f.debug_struct("Picture")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish()
    }
}

/// A picture read from disk.
#[derive(Clone, Debug)]
pub struct FilePicture {
    pub picture: Picture,
    /// The file's size in bytes.
    pub len: u64,
    pub stamp: Stamp,
}

/// How a picture is shown: fit to its space, or at a scale and moved by an
/// offset. Kept by the app, not the widget, so it survives the view being
/// rebuilt.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Zoom {
    /// Screen pixels per pixel of the picture; `None` fits it to the space.
    pub scale: Option<f32>,
    /// From the middle of the space to the middle of the picture.
    pub offset: Vector,
}

/// A picture open in place of the terminals.
#[derive(Debug)]
pub struct PictureDoc {
    pub file: FilePicture,
    pub zoom: Zoom,
    /// The file was deleted from disk.
    pub deleted: bool,
}

/// Reads the picture at `path`, or says why it can't be shown.
pub fn read(path: &Path) -> Result<FilePicture, String> {
    let kind = kind_of(path).ok_or("it is not a picture")?;
    let too_large = || format!("it is larger than {} MB", MAX_SIZE / (1024 * 1024));
    let (bytes, stamp) = disk::read_bytes(path, MAX_SIZE, too_large)?;
    let len = bytes.len() as u64;
    let picture = match kind {
        Kind::Raster => decode(&bytes)?,
        Kind::Vector => measure_svg(bytes)?,
    };
    Ok(FilePicture { picture, len, stamp })
}

/// Decodes a raster picture, turned upright as its camera says. The format
/// is read from the bytes, so a misnamed file still shows.
fn decode(bytes: &[u8]) -> Result<Picture, String> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("its format isn't one that can be shown")?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(orientation) = orientation {
        decoded.apply_orientation(orientation);
    }
    let has_alpha = decoded.color().has_alpha();
    let rgba = decoded.into_rgba8();
    let transparent = has_alpha && rgba.pixels().any(|p| p.0[3] < u8::MAX);
    let (width, height) = rgba.dimensions();
    let handle = raster::Handle::from_rgba(width, height, rgba.into_raw());
    Ok(Picture { image: Image::Raster(handle), width, height, format: format_name(format), transparent })
}

/// Checks that `bytes` are an SVG and reads its size.
fn measure_svg(bytes: Vec<u8>) -> Result<Picture, String> {
    let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let size = tree.size();
    Ok(Picture {
        width: (size.width().round() as u32).max(1),
        height: (size.height().round() as u32).max(1),
        image: Image::Vector(svg::Handle::from_memory(bytes)),
        format: "SVG",
        transparent: true,
    })
}

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WebP",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Ico => "ICO",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Tga => "TGA",
        ImageFormat::Qoi => "QOI",
        ImageFormat::Pnm => "PNM",
        _ => "Picture",
    }
}

/// A file's size for people: "812 B", "45 KB", "2.4 MB".
pub fn size_label(len: u64) -> String {
    const KB: f64 = 1024.0;
    let n = len as f64;
    if n < KB {
        format!("{len} B")
    } else if n < KB * KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{:.1} MB", n / (KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, alpha]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn pictures_are_known_by_their_extension() {
        assert_eq!(kind_of(Path::new("a/shot.PNG")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("photo.jpeg")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("logo.svg")), Some(Kind::Vector));
        assert_eq!(kind_of(Path::new("logo.svgz")), Some(Kind::Vector));
        assert_eq!(kind_of(Path::new("main.rs")), None);
        assert_eq!(kind_of(Path::new("png")), None, "a name, not an extension");
        assert!(has_text(Path::new("logo.SVG")));
        assert!(!has_text(Path::new("logo.svgz")), "gzipped");
        assert!(!has_text(Path::new("shot.png")));
    }

    #[test]
    fn raster_pictures_are_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, png(3, 2, 255)).unwrap();
        let read = read(&file).unwrap();
        let p = &read.picture;
        assert_eq!((p.width, p.height, p.format), (3, 2, "PNG"));
        assert!(!p.transparent, "every pixel is opaque");
        assert_eq!(read.len, std::fs::metadata(&file).unwrap().len());

        std::fs::write(&file, png(1, 1, 128)).unwrap();
        assert!(super::read(&file).unwrap().picture.transparent);
    }

    #[test]
    fn the_format_comes_from_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("really-a-png.jpg");
        std::fs::write(&file, png(2, 2, 255)).unwrap();
        assert_eq!(read(&file).unwrap().picture.format, "PNG");
    }

    #[test]
    fn broken_pictures_say_why() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.png");
        std::fs::write(&file, b"not a picture").unwrap();
        assert!(read(&file).is_err());
        let mut cut = png(4, 4, 255);
        cut.truncate(cut.len() / 2);
        std::fs::write(&file, cut).unwrap();
        assert!(read(&file).is_err(), "cut short");
        assert!(read(&dir.path().join("gone.png")).is_err());
        assert!(read(dir.path()).is_err(), "a folder");
    }

    #[test]
    fn svgs_are_measured() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.svg");
        let svg =
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="16"><rect width="24" height="16"/></svg>"#;
        std::fs::write(&file, svg).unwrap();
        let p = read(&file).unwrap().picture;
        assert_eq!((p.width, p.height, p.format), (24, 16, "SVG"));
        assert!(matches!(p.image, Image::Vector(_)));

        // Only a viewBox: its size.
        std::fs::write(&file, r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 50"/>"#).unwrap();
        let p = read(&file).unwrap().picture;
        assert_eq!((p.width, p.height), (100, 50));

        std::fs::write(&file, "<svg").unwrap();
        assert!(read(&file).is_err());
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(size_label(812), "812 B");
        assert_eq!(size_label(45 * 1024 + 100), "45 KB");
        assert_eq!(size_label(2_516_582), "2.4 MB");
    }
}
