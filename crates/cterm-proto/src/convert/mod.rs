//! Conversion utilities between cterm-core and proto types

pub mod color;
pub mod events;
pub mod frontend;
pub mod key;
pub mod screen;

pub use color::{color_to_proto, palette_to_proto, proto_to_color, proto_to_palette};
pub use events::event_to_proto;
pub use frontend::{frontend_state_to_proto, proto_to_frontend_state};
pub use key::{key_to_proto, modifiers_to_proto, proto_to_key, proto_to_modifiers};
pub use screen::{
    attrs_to_proto, cell_to_proto, cursor_style_to_proto, cursor_to_proto,
    extra_cursor_colors_to_proto, extra_cursors_to_proto, modes_to_proto, proto_to_attrs,
    proto_to_cursor_style, row_to_proto, screen_to_proto, screen_to_text, terminal_image_to_proto,
    terminal_images_to_proto, visible_row_to_proto, visible_rows_to_proto,
};
