//! Shared utility functions for UI components

/// Format a byte size for human-readable display
///
/// Returns a string like "1.5 KB", "2.3 MB", "1.0 GB", or "123 bytes"
pub fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    const GB: usize = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

/// Convert straight-alpha RGBA pixels to the native little-endian layout used
/// by Cairo ARGB32 surfaces and Direct2D premultiplied BGRA bitmaps.
///
/// Terminal images are stored as portable RGBA. Both non-Cocoa renderers use
/// premultiplied BGRA, so keeping the conversion here prevents the two native
/// backends from quietly disagreeing about alpha or channel order.
pub fn rgba_to_premultiplied_bgra(rgba: &[u8]) -> Option<Vec<u8>> {
    if !rgba.len().is_multiple_of(4) {
        return None;
    }

    let mut bgra = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        let alpha = u16::from(pixel[3]);
        let premultiply = |channel: u8| ((u16::from(channel) * alpha + 127) / 255) as u8;
        bgra.extend_from_slice(&[
            premultiply(pixel[2]),
            premultiply(pixel[1]),
            premultiply(pixel[0]),
            pixel[3],
        ]);
    }
    Some(bgra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(100), "100 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(1572864), "1.5 MB");
        assert_eq!(format_size(1073741824), "1.0 GB");
    }

    #[test]
    fn converts_rgba_to_premultiplied_bgra() {
        assert_eq!(
            rgba_to_premultiplied_bgra(&[255, 128, 0, 128, 10, 20, 30, 255]),
            Some(vec![0, 64, 128, 128, 30, 20, 10, 255])
        );
        assert_eq!(rgba_to_premultiplied_bgra(&[1, 2, 3]), None);
    }
}
