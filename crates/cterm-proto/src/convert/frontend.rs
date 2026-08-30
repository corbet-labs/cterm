//! Conversion of frontend-owned state used by terminal protocol reports.

use crate::proto::{FrontendTheme, FrontendVisibility};
use cterm_core::{FrontendState, ThemeAppearance, WindowVisibility};

pub fn frontend_state_to_proto(state: FrontendState) -> (i32, i32) {
    let theme = match state.appearance {
        ThemeAppearance::Dark => FrontendTheme::Dark,
        ThemeAppearance::Light => FrontendTheme::Light,
    };
    let visibility = match state.visibility {
        WindowVisibility::Visible => FrontendVisibility::Visible,
        WindowVisibility::Hidden => FrontendVisibility::Hidden,
    };
    (theme as i32, visibility as i32)
}

pub fn proto_to_frontend_state(theme: i32, visibility: i32) -> Option<FrontendState> {
    let appearance = match FrontendTheme::try_from(theme).ok()? {
        FrontendTheme::Unspecified | FrontendTheme::Dark => ThemeAppearance::Dark,
        FrontendTheme::Light => ThemeAppearance::Light,
    };
    let visibility = match FrontendVisibility::try_from(visibility).ok()? {
        FrontendVisibility::Unspecified | FrontendVisibility::Visible => WindowVisibility::Visible,
        FrontendVisibility::Hidden => WindowVisibility::Hidden,
    };
    Some(FrontendState {
        appearance,
        visibility,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_state_round_trips_and_defaults_old_clients() {
        let expected = FrontendState {
            appearance: ThemeAppearance::Light,
            visibility: WindowVisibility::Hidden,
        };
        let (theme, visibility) = frontend_state_to_proto(expected);
        assert_eq!(proto_to_frontend_state(theme, visibility), Some(expected));
        assert_eq!(
            proto_to_frontend_state(0, 0),
            Some(FrontendState::default())
        );
        assert_eq!(proto_to_frontend_state(99, 0), None);
        assert_eq!(proto_to_frontend_state(0, 99), None);
    }
}
