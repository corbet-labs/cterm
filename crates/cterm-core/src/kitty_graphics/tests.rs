use super::*;
use crate::screen::ScreenConfig;
use base64::engine::general_purpose::STANDARD;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;
#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::{Builder, NamedTempFile};

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
fn create_test_shared_memory(bytes: &[u8]) -> (String, std::fs::File) {
    static NEXT_NAME: AtomicU64 = AtomicU64::new(1);
    let name = format!(
        "/cterm-kitty-{}-{}",
        std::process::id(),
        NEXT_NAME.fetch_add(1, Ordering::Relaxed)
    );
    let descriptor = nix::sys::mman::shm_open(
        name.as_str(),
        nix::fcntl::OFlag::O_CREAT | nix::fcntl::OFlag::O_EXCL | nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    nix::unistd::ftruncate(&descriptor, bytes.len() as i64).unwrap();
    let file = std::fs::File::from(descriptor);
    // SAFETY: the test owns the descriptor and retains both the file and
    // mapping until the terminal has copied the bytes.
    let mut mapping = unsafe { memmap2::MmapOptions::new().map_mut(&file).unwrap() };
    // Darwin exposes POSIX shared-memory objects in page-sized mappings
    // even when ftruncate recorded a shorter logical payload.
    mapping[..bytes.len()].copy_from_slice(bytes);
    mapping.flush().unwrap();
    (name, file)
}

#[cfg(windows)]
fn create_test_shared_memory(bytes: &[u8]) -> (String, shared_memory::Shmem) {
    let mut mapping = shared_memory::ShmemConf::new()
        .size(bytes.len())
        .create()
        .unwrap();
    let name = mapping.get_os_id().to_owned();
    // SAFETY: this helper created and exclusively owns the mapping.
    unsafe { mapping.as_slice_mut().copy_from_slice(bytes) };
    (name, mapping)
}

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
fn test_shared_memory_exists(name: &str) -> bool {
    nix::sys::mman::shm_open(
        name,
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    )
    .is_ok()
}

fn sequence(control: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{control};{}\x1b\\", STANDARD.encode(payload)).into_bytes()
}

fn feed(graphics: &mut KittyGraphics, screen: &mut Screen, bytes: &[u8]) -> Vec<u8> {
    let mut forwarded = Vec::new();
    for byte in bytes {
        match graphics.advance(*byte) {
            InterceptorResult::Forward(bytes) => forwarded.extend_from_slice(bytes.as_slice()),
            InterceptorResult::Swallow => {}
            InterceptorResult::Captured(raw) => graphics.handle(&raw, screen),
        }
    }
    forwarded
}

#[test]
fn interceptor_is_lossless_for_non_kitty_sequences() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let input = b"a\x1b[31mb\x1b_not-kitty\x1b\\c";
    assert_eq!(feed(&mut graphics, &mut screen, input), input);
}

#[test]
fn direct_rgb_transmit_and_display_uses_shared_image_pipeline() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=24,s=2,v=2,i=7,C=1", &pixels),
    );

    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_eq!((images[0].pixel_width, images[0].pixel_height), (2, 2));
    assert_eq!(images[0].data.len(), 16);
    assert_eq!(
        screen.take_pending_responses(),
        vec![b"\x1b_Gi=7;OK\x1b\\".to_vec()]
    );
}

#[test]
fn unicode_placeholders_are_invisible_prototypes_that_follow_text_cells() {
    use crate::cell::KITTY_IMAGE_PLACEHOLDER;
    use crate::color::Color;

    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(4, 3, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    let pixels = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ];
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=2,v=2,i=1,p=9,U=1,c=2,r=2", &pixels),
    );
    assert!(screen.images().is_empty(), "the U=1 prototype is invisible");
    assert_eq!((screen.cursor.row, screen.cursor.col), (0, 0));

    for row in 0..2 {
        for col in 0..2 {
            let cell = screen.grid_mut().get_mut(row, col).unwrap();
            let coordinate = ['\u{0305}', '\u{030D}'];
            cell.set_text(&format!(
                "{KITTY_IMAGE_PLACEHOLDER}{}{}",
                coordinate[row], coordinate[col],
            ));
            cell.fg = Color::Indexed(1);
            cell.underline_color = Some(Color::Indexed(9));
        }
    }
    graphics.refresh_unicode_placements(&mut screen);
    let images = screen.images();
    assert_eq!(images.len(), 2, "each fused text row becomes one fragment");
    assert!(images.iter().all(|image| image.z_index == -1));
    assert_eq!(images[0].cell_width, 2);
    assert_eq!(images[0].data.as_slice(), &pixels[..8]);
    assert_eq!(images[1].data.as_slice(), &pixels[8..]);
    let stable_ids: Vec<u64> = images.iter().map(|image| image.id).collect();
    graphics.refresh_unicode_placements(&mut screen);
    assert_eq!(
        screen
            .images()
            .iter()
            .map(|image| image.id)
            .collect::<Vec<_>>(),
        stable_ids,
        "unchanged viewport projections reuse their RGBA fragments"
    );

    // Location-based deletion cannot touch virtual placements or the real
    // images derived from their text cells.
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=a\x1b\\");
    assert_eq!(screen.images().len(), 2);
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=i,i=1,p=9\x1b\\");
    assert!(screen.images().is_empty());
    assert!(graphics.virtual_placements.is_empty());
    assert!(graphics.images.contains_key(&1));
}

#[test]
fn unicode_placeholders_preserve_aspect_ratio_and_center_transparent_padding() {
    let mut screen = Screen::new(4, 3, ScreenConfig::default());
    screen.set_cell_width_hint(2.0);
    screen.set_cell_height_hint(2.0);
    let source = RgbaImage::from_raw(
        4,
        2,
        [[255, 0, 0, 255].repeat(4), [0, 0, 255, 255].repeat(4)].concat(),
    )
    .unwrap();
    let command = Command {
        columns: 2,
        rows: 2,
        ..Command::default()
    };
    let top = prepare_placeholder_fragment(
        &source,
        &command,
        PlaceholderRun {
            image_id: 1,
            placement_id: 0,
            image_row: 0,
            image_col: 0,
            screen_col: 0,
            columns: 2,
        },
        &screen,
    )
    .unwrap()
    .unwrap();
    let bottom = prepare_placeholder_fragment(
        &source,
        &command,
        PlaceholderRun {
            image_id: 1,
            placement_id: 0,
            image_row: 1,
            image_col: 0,
            screen_col: 0,
            columns: 2,
        },
        &screen,
    )
    .unwrap()
    .unwrap();

    assert_eq!((top.pixel_width, top.pixel_height), (4, 2));
    assert_eq!(&top.pixels[..16], &[0; 16]);
    assert_eq!(&top.pixels[16..], &[255, 0, 0, 255].repeat(4));
    assert_eq!(&bottom.pixels[..16], &[0, 0, 255, 255].repeat(4));
    assert_eq!(&bottom.pixels[16..], &[0; 16]);
}

#[test]
fn unicode_placeholder_fragments_follow_client_selected_animation_frames() {
    use crate::cell::KITTY_IMAGE_PLACEHOLDER;
    use crate::color::Color;

    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(2, 2, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=5,U=1,c=1,r=1", &[255, 0, 0, 255]),
    );
    let cell = screen.grid_mut().get_mut(0, 0).unwrap();
    cell.set_text(&format!("{KITTY_IMAGE_PLACEHOLDER}\u{0305}\u{0305}"));
    cell.fg = Color::Indexed(5);
    graphics.refresh_unicode_placements(&mut screen);
    assert_eq!(screen.images()[0].data.as_slice(), &[255, 0, 0, 255]);

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=5,f=32,s=1,v=1,X=1", &[0, 0, 255, 255]),
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=a,i=5,c=2\x1b\\");
    graphics.refresh_unicode_placements(&mut screen);

    assert_eq!(screen.images().len(), 1);
    assert_eq!(screen.images()[0].data.as_slice(), &[0, 0, 255, 255]);
}

#[test]
fn relative_placements_follow_named_parents_without_moving_the_cursor() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    screen.cursor.col = 1;
    screen.cursor.row = 1;
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=81,p=1,C=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=82", &[4, 5, 6, 255]),
    );
    let cursor = (screen.cursor.col, screen.cursor.row);
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=82,p=2,P=81,Q=1,H=2,V=1\x1b\\",
    );

    assert_eq!((screen.cursor.col, screen.cursor.row), cursor);
    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 82)
        .unwrap();
    assert_eq!((child.col, child.line), (3, 2));

    screen.cursor.col = 3;
    screen.cursor.row = 2;
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=81,p=1,C=1\x1b\\");
    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 82)
        .unwrap();
    assert_eq!((child.col, child.line), (5, 3));
}

#[test]
fn relative_placements_resolve_virtual_parent_placeholder_origins() {
    use crate::cell::KITTY_IMAGE_PLACEHOLDER;
    use crate::color::Color;

    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=83,p=3,U=1,c=1,r=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=84", &[4, 5, 6, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=84,p=4,P=83,Q=3,H=1,V=2\x1b\\",
    );
    assert!(screen.images().is_empty());

    let cell = screen.grid_mut().get_mut(1, 2).unwrap();
    cell.set_text(&format!("{KITTY_IMAGE_PLACEHOLDER}\u{0305}\u{0305}"));
    cell.fg = Color::Indexed(83);
    cell.underline_color = Some(Color::Indexed(3));
    graphics.refresh_unicode_placements(&mut screen);
    graphics.refresh_relative_placements(&mut screen);

    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 84)
        .unwrap();
    assert_eq!((child.col, child.line), (3, 3));
}

#[test]
fn relative_placements_reject_missing_parents_and_cycles() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    for id in 85..=87 {
        feed(
            &mut graphics,
            &mut screen,
            &sequence(&format!("a=t,f=32,s=1,v=1,i={id}"), &[1, 2, 3, 255]),
        );
    }
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=85,p=5,P=999,Q=1\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses().pop().unwrap())
            .unwrap()
            .contains("ENOPARENT")
    );

    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=85,p=5,C=1\x1b\\");
    screen.take_pending_responses();
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=85,p=5,P=85,Q=5\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses().pop().unwrap())
            .unwrap()
            .contains("EINVAL")
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=86,p=6,P=85,Q=5\x1b\\",
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=87,p=7,P=86,Q=6\x1b\\",
    );
    screen.take_pending_responses();
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=86,p=6,P=87,Q=7\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses().pop().unwrap())
            .unwrap()
            .contains("ECYCLE")
    );
}

#[test]
fn deleting_a_parent_cascades_relative_placements_and_orphaned_data() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    for id in 88..=90 {
        feed(
            &mut graphics,
            &mut screen,
            &sequence(&format!("a=t,f=32,s=1,v=1,i={id}"), &[1, 2, 3, 255]),
        );
    }
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=88,p=8,C=1\x1b\\");
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=89,p=9,P=88,Q=8,H=1\x1b\\",
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=90,p=10,P=89,Q=9,H=1\x1b\\",
    );
    assert_eq!(graphics.relative_placements.len(), 2);
    assert_eq!(screen.images().len(), 3);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=i,i=88,p=8\x1b\\");

    assert!(graphics.relative_placements.is_empty());
    assert!(screen.images().is_empty());
    assert!(graphics.images.contains_key(&88));
    assert!(!graphics.images.contains_key(&89));
    assert!(!graphics.images.contains_key(&90));
}

#[test]
fn erased_parent_screen_images_reap_relative_groups_on_refresh() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=96,p=16,C=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=97", &[4, 5, 6, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=97,p=17,P=96,Q=16\x1b\\",
    );
    let parent_screen_id = *graphics.placements.keys().next().unwrap();

    screen.remove_image(parent_screen_id);
    graphics.refresh_relative_placements(&mut screen);

    assert!(graphics.placements.is_empty());
    assert!(graphics.relative_placements.is_empty());
    assert!(!graphics.images.contains_key(&97));
}

#[test]
fn relative_placements_participate_in_geometric_and_freeing_deletes() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=101,p=21,C=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=102", &[4, 5, 6, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=102,p=22,P=101,Q=21,z=5\x1b\\",
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=z,z=5\x1b\\");
    assert!(graphics.relative_placements.is_empty());
    assert!(graphics.images.contains_key(&102));

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=102,p=22,P=101,Q=21,z=5\x1b\\",
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=Z,z=5\x1b\\");
    assert!(graphics.relative_placements.is_empty());
    assert!(!graphics.images.contains_key(&102));
    assert!(graphics.images.contains_key(&101));
}

#[test]
fn deleting_a_virtual_parent_cascades_hidden_relative_children() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=98,p=18,U=1,c=1,r=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=99", &[4, 5, 6, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=99,p=19,P=98,Q=18\x1b\\",
    );
    assert_eq!(graphics.relative_placements.len(), 1);
    assert!(screen.images().is_empty());

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=i,i=98,p=18\x1b\\");

    assert!(graphics.virtual_placements.is_empty());
    assert!(graphics.relative_placements.is_empty());
    assert!(graphics.images.contains_key(&98));
    assert!(!graphics.images.contains_key(&99));
}

#[test]
fn named_parent_type_changes_preserve_relative_children() {
    use crate::cell::KITTY_IMAGE_PLACEHOLDER;
    use crate::color::Color;

    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=91,p=11,C=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=92", &[4, 5, 6, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=92,p=12,P=91,Q=11,H=1\x1b\\",
    );
    assert_eq!(screen.images().len(), 2);

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=91,p=11,U=1,c=1,r=1\x1b\\",
    );
    assert!(screen.images().is_empty());
    assert_eq!(graphics.relative_placements.len(), 1);

    let cell = screen.grid_mut().get_mut(2, 2).unwrap();
    cell.set_text(&format!("{KITTY_IMAGE_PLACEHOLDER}\u{0305}\u{0305}"));
    cell.fg = Color::Indexed(91);
    cell.underline_color = Some(Color::Indexed(11));
    graphics.refresh_unicode_placements(&mut screen);
    graphics.refresh_relative_placements(&mut screen);
    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 92)
        .unwrap();
    assert_eq!((child.col, child.line), (3, 2));

    screen.cursor.col = 4;
    screen.cursor.row = 1;
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=91,p=11,C=1\x1b\\");
    assert!(graphics.virtual_placements.is_empty());
    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 92)
        .unwrap();
    assert_eq!((child.col, child.line), (5, 1));
}

#[test]
fn negative_relative_offsets_clip_whole_cells_from_the_image() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=93,p=13,C=1", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=2,v=1,i=94", &[10, 20, 30, 255, 40, 50, 60, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=94,p=14,P=93,Q=13,H=-1,c=2,r=1\x1b\\",
    );

    let child = screen
        .images()
        .into_iter()
        .find(|image| image.protocol_image_id == 94)
        .unwrap();
    assert_eq!((child.col, child.cell_width), (0, 1));
    assert_eq!(child.data.as_slice(), &[40, 50, 60, 255]);
}

#[test]
fn relative_chains_allow_32_ancestors_then_report_too_deep() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(8, 6, ScreenConfig::default());
    screen.set_cell_width_hint(1.0);
    screen.set_cell_height_hint(1.0);
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=95", &[1, 2, 3, 255]),
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=95,p=1,C=1\x1b\\");
    for placement_id in 2..=34 {
        feed(
            &mut graphics,
            &mut screen,
            format!(
                "\x1b_Ga=p,i=95,p={placement_id},P=95,Q={}\x1b\\",
                placement_id - 1
            )
            .as_bytes(),
        );
    }
    assert_eq!(graphics.relative_placements.len(), 33);
    screen.take_pending_responses();

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=95,p=35,P=95,Q=34\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses().pop().unwrap())
            .unwrap()
            .contains("ETOODEEP")
    );
    assert_eq!(graphics.relative_placements.len(), 33);
}

#[test]
fn zlib_compressed_rgba_is_inflated_before_display() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let pixels = [10, 20, 30, 40, 50, 60, 70, 80];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&pixels).unwrap();
    let compressed = encoder.finish().unwrap();

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=2,v=1,o=z,i=8,C=1", &compressed),
    );

    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].data.as_slice(), pixels);
}

#[test]
fn png_payload_is_decoded_into_the_shared_rgba_pipeline() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4AWOwBQAAPwA+Eq7IEAAAAABJRU5ErkJggg==")
            .unwrap();

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=100,i=12,C=1", &png),
    );

    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_eq!((images[0].pixel_width, images[0].pixel_height), (1, 1));
    assert_eq!(images[0].data.len(), 4);
}

#[test]
fn file_transfer_obeys_offset_and_size_without_deleting_source() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(&[99, 98, 1, 2, 3, 4, 97]).unwrap();
    file.flush().unwrap();
    let path = file.path().to_owned();

    feed(
        &mut graphics,
        &mut screen,
        &sequence(
            "a=T,t=f,f=32,s=1,v=1,O=2,S=4,i=13,C=1",
            path.as_os_str().as_encoded_bytes(),
        ),
    );

    assert!(path.exists());
    assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    windows
))]
#[test]
fn shared_memory_transfer_obeys_range_and_cleanup_semantics() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let (name, _mapping) = create_test_shared_memory(&[99, 98, 1, 2, 3, 4, 97]);

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,t=s,f=32,s=1,v=1,O=2,S=4,i=14,C=1", name.as_bytes()),
    );

    assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
    assert_eq!(
        screen.take_pending_responses(),
        vec![b"\x1b_Gi=14;OK\x1b\\".to_vec()]
    );
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
    assert!(!test_shared_memory_exists(&name));
}

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
#[test]
fn shared_memory_errors_still_unlink_the_posix_object() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let (name, _mapping) = create_test_shared_memory(&[1, 2, 3, 4]);

    feed(
        &mut graphics,
        &mut screen,
        &sequence(
            "a=T,t=s,f=32,s=1,v=1,O=4294967295,i=15,C=1",
            name.as_bytes(),
        ),
    );

    assert!(screen.images().is_empty());
    assert_eq!(
        screen.take_pending_responses(),
        vec![b"\x1b_Gi=15;EBADF:shared memory offset is out of range\x1b\\".to_vec()]
    );
    assert!(!test_shared_memory_exists(&name));
}

#[test]
fn protocol_named_temp_transfer_is_removed_after_reading() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let mut file = Builder::new()
        .prefix("tty-graphics-protocol-")
        .tempfile()
        .unwrap();
    file.write_all(&[1, 2, 3, 4]).unwrap();
    file.flush().unwrap();
    let (handle, path) = file.keep().unwrap();
    drop(handle);

    feed(
        &mut graphics,
        &mut screen,
        &sequence(
            "a=T,t=t,f=32,s=1,v=1,i=14,C=1",
            path.as_os_str().as_encoded_bytes(),
        ),
    );

    assert!(!path.exists());
    assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
}

#[test]
fn crop_scale_and_offsets_produce_bounded_placement_geometry() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    screen.set_cell_width_hint(4.0);
    screen.set_cell_height_hint(4.0);
    let pixels = [1, 2, 3, 4, 10, 20, 30, 40, 5, 6, 7, 8, 50, 60, 70, 80];

    feed(
        &mut graphics,
        &mut screen,
        &sequence(
            "a=T,f=32,s=2,v=2,x=1,y=0,w=1,h=2,c=1,r=1,X=1,Y=2,i=15,C=1",
            &pixels,
        ),
    );

    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_eq!((images[0].pixel_width, images[0].pixel_height), (5, 6));
    assert_eq!((images[0].cell_width, images[0].cell_height), (1, 1));
    assert_eq!(&images[0].data[..4 * 5 * 2], &[0; 4 * 5 * 2]);
    assert_eq!(&images[0].data[4 * 5 * 2..4 * 5 * 2 + 4], &[0; 4]);
    assert_ne!(&images[0].data[4 * 5 * 2 + 4..4 * 5 * 2 + 8], &[0; 4]);
    assert_eq!(graphics.placement_bytes, 5 * 6 * 4);
}

#[test]
fn placement_cursor_motion_uses_the_full_rectangle_and_wrap_policy() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(6, 6, ScreenConfig::default());
    screen.cursor.col = 1;
    screen.cursor.row = 1;
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,c=2,r=3,i=16", &[1, 2, 3, 4]),
    );
    assert_eq!((screen.cursor.col, screen.cursor.row), (3, 3));

    screen.cursor.col = 5;
    screen.cursor.row = 1;
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=16,c=2,r=2\x1b\\");
    assert_eq!((screen.cursor.col, screen.cursor.row), (0, 3));
}

#[test]
fn chunked_upload_and_later_placement_preserve_identity() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let encoded = STANDARD.encode([1, 2, 3, 255]);
    let first = format!("\x1b_Ga=t,f=32,s=1,v=1,i=9,m=1;{}\x1b\\", &encoded[..4]);
    let second = format!("\x1b_Gm=0;{}\x1b\\", &encoded[4..]);
    feed(&mut graphics, &mut screen, first.as_bytes());
    assert!(screen.images().is_empty());
    feed(&mut graphics, &mut screen, second.as_bytes());
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=9,p=4,C=1\x1b\\");
    assert_eq!(screen.images().len(), 1);
    assert_eq!(screen.take_pending_responses().len(), 2);
}

#[test]
fn animation_frame_uses_contextual_fields_and_preserves_existing_placement() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    let root = [10, 20, 30, 255, 40, 50, 60, 255];
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=2,v=1,i=71,C=1", &root),
    );

    // The action deliberately follows z=: parsing must still interpret z
    // as a frame gap, X as overwrite, and Y as an RGBA background.
    feed(
        &mut graphics,
        &mut screen,
        &sequence(
            "z=75,a=f,i=71,f=32,s=1,v=1,x=1,y=0,X=1,Y=4278190335",
            &[1, 2, 3, 255],
        ),
    );
    let image = graphics.images.get(&71).unwrap();
    assert_eq!(image.frames.len(), 2);
    assert_eq!(image.frames[1].gap_ms, 75);
    assert_eq!(
        image.frames[1].rgba.as_slice(),
        [255, 0, 0, 255, 1, 2, 3, 255]
    );
    assert_eq!(image.animation_state, AnimationState::Stopped);
    let second_frame = Arc::clone(&image.frames[1].rgba);

    // Client-driven frame selection updates the existing screen placement,
    // rather than creating a second image or requiring renderer knowledge.
    feed(&mut graphics, &mut screen, b"\x1b_Ga=a,i=71,c=2\x1b\\");
    assert_eq!(screen.images().len(), 1);
    assert_eq!(screen.images()[0].data.as_slice(), second_frame.as_slice());
    assert_eq!(screen.take_pending_responses().len(), 2);
}

#[test]
fn frame_edit_and_composition_follow_kitty_r_c_and_offset_semantics() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=2,v=1,i=72", &[10, 20, 30, 255, 40, 50, 60, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=72,f=32,s=2,v=1,X=1", &[1, 2, 3, 255, 4, 5, 6, 255]),
    );

    // r= is the source frame, c= the destination; X/Y are source
    // offsets, x/y destination offsets, and C=1 means overwrite.
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=c,i=72,r=2,c=1,X=1,Y=0,x=0,y=0,w=1,h=1,C=1\x1b\\",
    );
    assert_eq!(
        graphics.images[&72].frames[0].rgba.as_slice(),
        [4, 5, 6, 255, 40, 50, 60, 255]
    );

    // r=2 edits frame 2 on its own canvas; z=0 is ignored and retains
    // the existing default 40ms gap.
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=72,r=2,f=32,s=1,v=1,x=1,X=1,z=0", &[9, 8, 7, 255]),
    );
    let edited = &graphics.images[&72].frames[1];
    assert_eq!(edited.rgba.as_slice(), [1, 2, 3, 255, 9, 8, 7, 255]);
    assert_eq!(edited.gap_ms, 40);
}

#[test]
fn transient_usage_hints_propagate_through_frame_composition() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=76,N=2", &[1, 2, 3, 255]),
    );
    assert!(!graphics.images[&76].frames[0].transient);

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=76,f=32,s=1,v=1,X=1,N=1", &[4, 5, 6, 255]),
    );
    assert!(graphics.images[&76].frames[1].transient);

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=c,i=76,r=2,c=1,C=1\x1b\\",
    );
    assert!(graphics.images[&76].frames[0].transient);

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=76,c=1,f=32,s=1,v=1,X=1", &[7, 8, 9, 255]),
    );
    assert!(
        graphics.images[&76].frames[2].transient,
        "a frame based on transient data inherits the hint"
    );
}

#[test]
fn transient_unplaced_images_are_evicted_before_older_regular_images() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=77", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=78,N=1", &[4, 5, 6, 255]),
    );

    graphics.evict_to_fit(STORE_QUOTA_BYTES - 4, None, &mut screen);

    assert!(graphics.images.contains_key(&77));
    assert!(!graphics.images.contains_key(&78));
}

#[test]
fn composition_rejects_missing_frames_bounds_and_same_frame_overlap() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=73", &[1, 2, 3, 255]),
    );
    screen.take_pending_responses();

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=c,i=73,r=2,c=1,w=1,h=1\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses()[0].clone())
            .unwrap()
            .contains("ENOENT")
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=c,i=73,r=1,c=1,w=1,h=1\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses()[0].clone())
            .unwrap()
            .contains("EINVAL")
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=c,i=73,r=1,c=1,x=4294967295,w=1,h=1\x1b\\",
    );
    assert!(
        String::from_utf8(screen.take_pending_responses()[0].clone())
            .unwrap()
            .contains("EINVAL")
    );
}

#[test]
fn terminal_driven_animation_honors_gaps_loading_and_loop_limits() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=74,C=1", &[10, 20, 30, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=f,i=74,f=32,s=1,v=1,X=1,z=40", &[1, 2, 3, 255]),
    );
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=a,i=74,r=1,z=10,s=3,v=2\x1b\\",
    );

    assert_eq!(
        graphics.advance_animations(0, &mut screen).next_wake_ms,
        Some(10)
    );
    let second = graphics.advance_animations(10, &mut screen);
    assert!(second.changed);
    assert_eq!(second.next_wake_ms, Some(50));
    assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 255]);
    let stopped = graphics.advance_animations(50, &mut screen);
    assert!(!stopped.changed);
    assert_eq!(stopped.next_wake_ms, None);
    assert_eq!(
        graphics.images[&74].animation_state,
        AnimationState::Stopped
    );

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=a,i=74,c=2,s=2,v=1\x1b\\",
    );
    let waiting = graphics.advance_animations(100, &mut screen);
    assert!(!waiting.changed);
    assert_eq!(waiting.next_wake_ms, Some(140));
    assert_eq!(
        graphics.advance_animations(140, &mut screen).next_wake_ms,
        None
    );
    assert_eq!(
        graphics.images[&74].animation_state,
        AnimationState::Loading
    );
}

#[test]
fn invalid_animation_states_are_rejected_instead_of_silently_ignored() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=75", &[1, 2, 3, 255]),
    );
    screen.take_pending_responses();

    feed(&mut graphics, &mut screen, b"\x1b_Ga=a,i=75,s=9\x1b\\");

    assert!(
        String::from_utf8(screen.take_pending_responses().pop().unwrap())
            .unwrap()
            .contains("EINVAL")
    );
    assert_eq!(
        graphics.images[&75].animation_state,
        AnimationState::Stopped
    );
}

#[test]
fn query_replies_without_storing_the_probe() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=q,f=24,s=1,v=1,i=31", &[0, 0, 0]),
    );
    assert!(screen.images().is_empty());
    assert!(graphics.images.is_empty());
    assert_eq!(
        screen.take_pending_responses(),
        vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]
    );
}

#[test]
fn uppercase_delete_removes_placement_and_image_data() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=5,C=1", &[1, 2, 3, 4]),
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=5\x1b\\");
    assert!(screen.images().is_empty());
    assert!(graphics.images.is_empty());
}

#[test]
fn replacing_a_placement_removes_the_previous_screen_image() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=21,p=2,C=1", &[1, 2, 3, 4]),
    );
    let first_screen_id = screen.images()[0].id;

    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=21,p=2,C=1\x1b\\");

    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_ne!(images[0].id, first_screen_id);
}

#[test]
fn anonymous_placements_accumulate_while_named_placements_replace() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=40", &[1, 2, 3, 4]),
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,C=1\x1b\\");
    screen.cursor.col = 2;
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,C=1\x1b\\");
    assert_eq!(screen.images().len(), 2);
    assert_eq!(graphics.placements.len(), 2);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,p=7,C=1\x1b\\");
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,p=7,C=1\x1b\\");
    assert_eq!(screen.images().len(), 3);
    assert_eq!(graphics.placements.len(), 3);
}

#[test]
fn image_delete_can_target_one_named_placement_without_freeing_shared_data() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=50", &[1, 2, 3, 4]),
    );
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=50,p=1,C=1\x1b\\");
    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=50,p=2,C=1\x1b\\");

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=50,p=1\x1b\\");
    assert_eq!(screen.images().len(), 1);
    assert!(graphics.images.contains_key(&50));
    assert_eq!(graphics.placements.len(), 1);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=50,p=2\x1b\\");
    assert!(screen.images().is_empty());
    assert!(!graphics.images.contains_key(&50));
}

#[test]
fn image_numbers_keep_history_and_delete_only_the_newest_match() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,I=9", &[1, 2, 3, 4]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,I=9", &[5, 6, 7, 8]),
    );
    assert_eq!(graphics.images.len(), 2);
    assert_eq!(graphics.image_numbers.get(&9).unwrap().len(), 2);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,I=9,p=1,C=1\x1b\\");
    assert_eq!(screen.images()[0].data.as_slice(), [5, 6, 7, 8]);
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=N,I=9\x1b\\");
    assert_eq!(graphics.images.len(), 1);
    assert_eq!(graphics.image_numbers.get(&9).unwrap().len(), 1);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=p,I=9,p=2,C=1\x1b\\");
    assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
}

#[test]
fn geometric_and_z_delete_selectors_match_only_intersecting_placements() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=60", &[1, 2, 3, 4]),
    );
    screen.cursor.col = 1;
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=60,p=1,z=-1,C=1\x1b\\",
    );
    screen.cursor.col = 4;
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=p,i=60,p=2,z=3,C=1\x1b\\",
    );

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=d,d=q,x=2,y=1,z=-1\x1b\\",
    );
    assert_eq!(screen.images().len(), 1);
    assert_eq!(graphics.placements.values().next().unwrap().z_index, 3);

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=x,x=5\x1b\\");
    assert!(screen.images().is_empty());
    assert!(graphics.images.contains_key(&60));
}

#[test]
fn placement_preserves_text_and_exposes_renderer_layer_metadata() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    screen.put_char('X');
    screen.cursor.col = 0;

    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=70,z=-1,C=1", &[1, 2, 3, 4]),
    );

    assert_eq!(screen.get_cell(0, 0).unwrap().text(), "X");
    let image = screen.images()[0];
    assert_eq!(image.z_index, -1);
    assert_eq!(image.protocol_image_id, 70);
    assert_eq!(image.layer(), crate::ImageLayer::BehindText);
}

#[test]
fn cursor_row_and_id_range_delete_selectors_are_independent() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    screen.cursor.col = 1;
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=80,p=1,C=1", &[1, 2, 3, 4]),
    );
    screen.cursor.col = 4;
    screen.cursor.row = 1;
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=81,p=1,C=1", &[5, 6, 7, 8]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=82", &[9, 10, 11, 12]),
    );

    screen.cursor.col = 1;
    screen.cursor.row = 0;
    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=c\x1b\\");
    assert_eq!(screen.images().len(), 1);
    assert!(graphics.images.contains_key(&80));

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=y,y=2\x1b\\");
    assert!(screen.images().is_empty());
    assert!(graphics.images.contains_key(&81));

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=R,x=80,y=81\x1b\\");
    assert!(!graphics.images.contains_key(&80));
    assert!(!graphics.images.contains_key(&81));
    assert!(graphics.images.contains_key(&82));
}

#[test]
fn delete_all_affects_the_live_viewport_but_preserves_history_placements() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 3, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=70,C=1", &[1, 2, 3, 4]),
    );
    screen.cursor.row = 2;
    screen.line_feed();
    screen.cursor.row = 2;
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=71,C=1", &[5, 6, 7, 8]),
    );

    feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=A\x1b\\");
    let images = screen.images();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].line, 0);
    assert!(graphics.images.contains_key(&70));
    assert!(!graphics.images.contains_key(&71));
}

#[test]
fn quota_eviction_preserves_visible_images_and_reconciles_cleared_placements() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=t,f=32,s=1,v=1,i=30", &[1, 2, 3, 4]),
    );
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=31,C=1", &[5, 6, 7, 8]),
    );

    graphics.evict_to_fit(STORE_QUOTA_BYTES, None, &mut screen);
    assert!(!graphics.images.contains_key(&30));
    assert!(graphics.images.contains_key(&31));
    assert_eq!(screen.images().len(), 1);

    screen.clear_images();
    graphics.evict_to_fit(STORE_QUOTA_BYTES, None, &mut screen);
    assert!(graphics.images.is_empty());
    assert!(graphics.placements.is_empty());
    assert_eq!(graphics.placement_bytes, 0);
}

#[test]
fn rgb_expansion_is_rejected_before_it_can_exceed_the_decoded_budget() {
    let command = Command {
        format: Format::Rgb24,
        pixel_width: Some(10_000),
        pixel_height: Some(1_700),
        ..Command::default()
    };

    let error = decode_raw(&command, Vec::new(), command.echo()).unwrap_err();
    assert_eq!(error.code, ErrorCode::NoSpace);
}

#[test]
fn quiet_levels_suppress_success_and_then_error_replies() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        &sequence("a=T,f=32,s=1,v=1,i=22,q=1,C=1", &[1, 2, 3, 4]),
    );
    assert!(screen.take_pending_responses().is_empty());

    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=T,f=32,s=2,v=2,i=23,q=2;AAAA\x1b\\",
    );
    assert!(screen.take_pending_responses().is_empty());
}

#[test]
fn invalid_payload_returns_a_bounded_protocol_error() {
    let mut graphics = KittyGraphics::default();
    let mut screen = Screen::new(10, 5, ScreenConfig::default());
    feed(
        &mut graphics,
        &mut screen,
        b"\x1b_Ga=T,f=24,s=2,v=2,i=3;AAAA\x1b\\",
    );
    let reply = screen.take_pending_responses().pop().unwrap();
    assert!(String::from_utf8(reply).unwrap().contains("ENODATA"));
    assert!(screen.images().is_empty());
}
