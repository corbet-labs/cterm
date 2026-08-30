//! Color conversion between cterm-core and proto

use crate::proto;

fn rgb_to_proto(color: cterm_core::Rgb) -> proto::Rgb {
    proto::Rgb {
        r: u32::from(color.r),
        g: u32::from(color.g),
        b: u32::from(color.b),
    }
}

fn proto_to_rgb(color: &proto::Rgb) -> Option<cterm_core::Rgb> {
    Some(cterm_core::Rgb::new(
        u8::try_from(color.r).ok()?,
        u8::try_from(color.g).ok()?,
        u8::try_from(color.b).ok()?,
    ))
}

/// Convert a complete core palette to its wire representation.
pub fn palette_to_proto(palette: &cterm_core::ColorPalette) -> proto::FrontendPalette {
    proto::FrontendPalette {
        ansi: palette.ansi.iter().copied().map(rgb_to_proto).collect(),
        foreground: Some(rgb_to_proto(palette.foreground)),
        background: Some(rgb_to_proto(palette.background)),
        cursor: Some(rgb_to_proto(palette.cursor)),
        selection: Some(rgb_to_proto(palette.selection)),
    }
}

/// Validate and convert a frontend palette received over the wire.
pub fn proto_to_palette(palette: &proto::FrontendPalette) -> Option<cterm_core::ColorPalette> {
    let ansi: [cterm_core::Rgb; 16] = palette
        .ansi
        .iter()
        .map(proto_to_rgb)
        .collect::<Option<Vec<_>>>()?
        .try_into()
        .ok()?;
    Some(cterm_core::ColorPalette {
        ansi,
        foreground: proto_to_rgb(palette.foreground.as_ref()?)?,
        background: proto_to_rgb(palette.background.as_ref()?)?,
        cursor: proto_to_rgb(palette.cursor.as_ref()?)?,
        selection: proto_to_rgb(palette.selection.as_ref()?)?,
    })
}

/// Convert cterm_core::Color to proto::Color
pub fn color_to_proto(color: &cterm_core::Color) -> proto::Color {
    use proto::color::ColorType;

    let color_type = match color {
        cterm_core::Color::Default => Some(ColorType::Default(true)),
        cterm_core::Color::Ansi(ansi) => Some(ColorType::Ansi(*ansi as u32)),
        cterm_core::Color::Indexed(idx) => Some(ColorType::Indexed(*idx as u32)),
        cterm_core::Color::Rgb(rgb) => Some(ColorType::Rgb(proto::Rgb {
            r: rgb.r as u32,
            g: rgb.g as u32,
            b: rgb.b as u32,
        })),
    };

    proto::Color { color_type }
}

/// Convert proto::Color to cterm_core::Color
pub fn proto_to_color(color: &proto::Color) -> cterm_core::Color {
    use proto::color::ColorType;

    match &color.color_type {
        Some(ColorType::Default(_)) => cterm_core::Color::Default,
        Some(ColorType::Ansi(idx)) => {
            if let Some(ansi) = cterm_core::AnsiColor::from_index(*idx as u8) {
                cterm_core::Color::Ansi(ansi)
            } else {
                cterm_core::Color::Default
            }
        }
        Some(ColorType::Indexed(idx)) => cterm_core::Color::Indexed(*idx as u8),
        Some(ColorType::Rgb(rgb)) => {
            cterm_core::Color::Rgb(cterm_core::Rgb::new(rgb.r as u8, rgb.g as u8, rgb.b as u8))
        }
        None => cterm_core::Color::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_color_roundtrip() {
        let color = cterm_core::Color::Default;
        let proto = color_to_proto(&color);
        let back = proto_to_color(&proto);
        assert_eq!(color, back);
    }

    #[test]
    fn test_ansi_color_roundtrip() {
        let color = cterm_core::Color::Ansi(cterm_core::AnsiColor::Red);
        let proto = color_to_proto(&color);
        let back = proto_to_color(&proto);
        assert_eq!(color, back);
    }

    #[test]
    fn test_rgb_color_roundtrip() {
        let color = cterm_core::Color::rgb(128, 64, 255);
        let proto = color_to_proto(&color);
        let back = proto_to_color(&proto);
        assert_eq!(color, back);
    }

    #[test]
    fn test_palette_roundtrip_and_validation() {
        let palette = cterm_core::ColorPalette::default_light();
        let proto = palette_to_proto(&palette);
        let restored = proto_to_palette(&proto).unwrap();
        assert_eq!(restored.ansi, palette.ansi);
        assert_eq!(restored.foreground, palette.foreground);
        assert_eq!(restored.background, palette.background);
        assert_eq!(restored.cursor, palette.cursor);
        assert_eq!(restored.selection, palette.selection);

        let mut incomplete = proto;
        incomplete.ansi.pop();
        assert!(proto_to_palette(&incomplete).is_none());
    }
}
