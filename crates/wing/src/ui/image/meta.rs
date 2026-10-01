//! Image metadata read from the **file header only**.
//!
//! The header carries everything layout needs (pixel dimensions → aspect ratio, byte size,
//! mtime) without decoding a single pixel, which is what makes "metadata first, pixels
//! later" affordable on a background thread.
//!
//! [`probe`] does I/O and is therefore **worker-thread only**: the store calls it, the UI
//! thread never does. Everything else in this module is plain data.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::ImageReader;

use super::store::Limits;

/// What the file header says about an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageMeta {
    /// Image width in pixels.
    pub px_w: u32,
    /// Image height in pixels.
    pub px_h: u32,
    /// File size in bytes (not the decoded size).
    pub bytes: u64,
    /// Modification time, part of the cache identity. `None` if the platform refuses to
    /// report one — the cache then falls back to "path + target size" as its identity.
    pub mtime: Option<SystemTime>,
}

impl ImageMeta {
    /// `px_w / px_h` (as `f64`). Degenerate headers (zero height) answer `0.0` rather than
    /// an infinity, so a caller's row math stays finite.
    pub fn aspect_ratio(&self) -> f64 {
        if self.px_h == 0 {
            return 0.0;
        }
        f64::from(self.px_w) / f64::from(self.px_h)
    }
}

/// Why an image cannot be rendered.
///
/// Every failure path in this module ends in one of these instead of an error, a panic or a
/// blank frame: the caller's contract is "anything other than `Ready` keeps the existing
/// text rendering", so a reason only has to be good enough to log or show in a status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// No graphics protocol (or images turned off by config).
    Disabled,
    /// The caller asked for a zero-sized target — there is nowhere to draw.
    NoSpace,
    /// The path does not exist.
    Missing,
    /// The path exists but is not a regular file (directory, socket, device…).
    NotAFile,
    /// The file is zero bytes long.
    Empty,
    /// The file exceeds [`Limits::file_bytes`].
    TooLarge {
        /// Size on disk, in bytes.
        bytes: u64,
    },
    /// The file could not be opened or read (permissions, I/O error).
    Unreadable,
    /// The header did not parse: not an image, corrupt, or a format whose codec is not
    /// compiled in (`image` is built with png + jpeg only). Also what a decoder *panic* is
    /// reported as — the worker catches it and fails this one image (see [`super::store`]).
    NotAnImage,
    /// The header parsed but the pixel count exceeds [`Limits::pixels`] — refusing to
    /// allocate is the only safe answer for a 100000×100000 header.
    TooManyPixels {
        /// Header width in pixels.
        px_w: u32,
        /// Header height in pixels.
        px_h: u32,
    },
    /// The terminal protocol encoder rejected the decoded image (a panic in the encoder is
    /// reported this way too).
    EncodeFailed,
    /// The worker thread is gone: no further probe or encode will ever be answered.
    ///
    /// Distinct from [`Unavailable::Disabled`] on purpose — the terminal *can* draw, but this
    /// store cannot produce anything any more (a job panicked past the guard, or the host's
    /// waker panicked and the process is in a bad state). The pipeline never pretends to be
    /// healthy: this reason is sticky and observable, and the caller's fallback is the same
    /// text rendering as everywhere else.
    WorkerFailed,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("no terminal graphics support"),
            Self::NoSpace => f.write_str("no room to draw the image"),
            Self::Missing => f.write_str("file not found"),
            Self::NotAFile => f.write_str("not a regular file"),
            Self::Empty => f.write_str("file is empty"),
            Self::TooLarge { bytes } => write!(f, "file is too large ({bytes} bytes)"),
            Self::Unreadable => f.write_str("file is not readable"),
            Self::NotAnImage => f.write_str("not a supported image"),
            Self::TooManyPixels { px_w, px_h } => {
                write!(f, "image is too large ({px_w}x{px_h} pixels)")
            }
            Self::EncodeFailed => f.write_str("terminal encoder rejected the image"),
            Self::WorkerFailed => f.write_str("the image pipeline stopped working"),
        }
    }
}

/// A successfully probed file: its metadata plus the canonical path used as its identity.
#[derive(Debug, Clone)]
pub(crate) struct Probed {
    pub(crate) canonical: PathBuf,
    pub(crate) meta: ImageMeta,
}

/// Read `path`'s header and report what it says. **Worker-thread only** (does I/O).
///
/// Never panics, never decodes pixels, and never returns a partially-populated
/// [`ImageMeta`]: each check either produces the complete struct or one [`Unavailable`].
pub(crate) fn probe(path: &Path, limits: &Limits) -> Result<Probed, Unavailable> {
    let stat = fs::metadata(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => Unavailable::Missing,
        _ => Unavailable::Unreadable,
    })?;
    if !stat.is_file() {
        return Err(Unavailable::NotAFile);
    }
    let bytes = stat.len();
    if bytes == 0 {
        return Err(Unavailable::Empty);
    }
    if bytes > limits.file_bytes {
        return Err(Unavailable::TooLarge { bytes });
    }
    // Separate the two ways the header can fail, because they mean different things to the
    // user: "the file is unreadable" vs "the file is not an image we know".
    let reader = ImageReader::open(path).map_err(|_| Unavailable::Unreadable)?;
    let reader = reader
        .with_guessed_format()
        .map_err(|_| Unavailable::Unreadable)?;
    // `into_dimensions` constructs the decoder and reads its header — it does not decode
    // pixels (that is the whole point of the two-phase probe/encode split).
    let (px_w, px_h) = reader
        .into_dimensions()
        .map_err(|_| Unavailable::NotAnImage)?;
    if u64::from(px_w) * u64::from(px_h) > limits.pixels {
        return Err(Unavailable::TooManyPixels { px_w, px_h });
    }

    Ok(Probed {
        canonical: fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        meta: ImageMeta {
            px_w,
            px_h,
            bytes,
            mtime: stat.modified().ok(),
        },
    })
}

/// Write a header-only PNG of the given size to `path`. Test/demo fixture helper.
#[cfg(test)]
pub(crate) fn write_png_fixture(path: &Path, px_w: u32, px_h: u32) {
    let img = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(px_w, px_h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    }));
    img.save_with_format(path, image::ImageFormat::Png)
        .expect("write png fixture");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::image::test_support::TempDir;

    fn probe_default(path: &Path) -> Result<Probed, Unavailable> {
        probe(path, &Limits::default())
    }

    #[test]
    fn reads_dimensions_from_a_png_header() {
        let dir = TempDir::new("meta-png");
        let path = dir.path().join("plot.png");
        write_png_fixture(&path, 320, 200);

        let probed = probe_default(&path).expect("probe");
        assert_eq!(probed.meta.px_w, 320);
        assert_eq!(probed.meta.px_h, 200);
        assert!(probed.meta.bytes > 0);
        assert!(probed.meta.mtime.is_some());
        assert!(probed.canonical.is_absolute());
        assert!((probed.meta.aspect_ratio() - 1.6).abs() < 1e-9);
    }

    #[test]
    fn reads_dimensions_from_a_jpeg_header() {
        let dir = TempDir::new("meta-jpeg");
        let path = dir.path().join("photo.jpg");
        let img = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            64,
            48,
            image::Rgb([10, 20, 30]),
        ));
        img.save_with_format(&path, image::ImageFormat::Jpeg)
            .expect("write jpeg fixture");

        let probed = probe_default(&path).expect("probe");
        assert_eq!((probed.meta.px_w, probed.meta.px_h), (64, 48));
    }

    #[test]
    fn missing_path_is_unavailable() {
        let dir = TempDir::new("meta-missing");
        let err = probe_default(&dir.path().join("nope.png")).unwrap_err();
        assert_eq!(err, Unavailable::Missing);
    }

    #[test]
    fn directory_is_unavailable() {
        let dir = TempDir::new("meta-dir");
        let err = probe_default(dir.path()).unwrap_err();
        assert_eq!(err, Unavailable::NotAFile);
    }

    #[test]
    fn empty_file_is_unavailable() {
        let dir = TempDir::new("meta-empty");
        let path = dir.path().join("empty.png");
        fs::write(&path, b"").expect("write");
        assert_eq!(probe_default(&path).unwrap_err(), Unavailable::Empty);
    }

    #[test]
    fn text_disguised_as_png_is_unavailable() {
        let dir = TempDir::new("meta-text");
        let path = dir.path().join("liar.png");
        fs::write(&path, b"this is not a png, it is a confession\n").expect("write");
        assert_eq!(probe_default(&path).unwrap_err(), Unavailable::NotAnImage);
    }

    #[test]
    fn oversize_file_is_rejected_before_any_header_read() {
        let dir = TempDir::new("meta-large");
        let path = dir.path().join("huge.png");
        fs::write(&path, vec![0u8; 4096]).expect("write");
        let limits = Limits {
            file_bytes: 1024,
            ..Limits::default()
        };
        assert_eq!(
            probe(&path, &limits).unwrap_err(),
            Unavailable::TooLarge { bytes: 4096 }
        );
    }

    #[test]
    fn too_many_pixels_is_rejected_from_the_header_alone() {
        let dir = TempDir::new("meta-pixels");
        let path = dir.path().join("wide.png");
        write_png_fixture(&path, 64, 64);
        let limits = Limits {
            pixels: 100,
            ..Limits::default()
        };
        assert_eq!(
            probe(&path, &limits).unwrap_err(),
            Unavailable::TooManyPixels { px_w: 64, px_h: 64 }
        );
    }

    #[test]
    fn aspect_ratio_of_degenerate_header_is_finite() {
        let meta = ImageMeta {
            px_w: 100,
            px_h: 0,
            bytes: 1,
            mtime: None,
        };
        assert_eq!(meta.aspect_ratio(), 0.0);
        assert!(meta.aspect_ratio().is_finite());
    }

    #[test]
    fn every_reason_has_a_human_readable_label() {
        let reasons = [
            Unavailable::Disabled,
            Unavailable::NoSpace,
            Unavailable::Missing,
            Unavailable::NotAFile,
            Unavailable::Empty,
            Unavailable::TooLarge { bytes: 7 },
            Unavailable::Unreadable,
            Unavailable::NotAnImage,
            Unavailable::TooManyPixels { px_w: 1, px_h: 2 },
            Unavailable::EncodeFailed,
            Unavailable::WorkerFailed,
        ];
        for reason in reasons {
            assert!(!reason.to_string().is_empty());
        }
    }
}
