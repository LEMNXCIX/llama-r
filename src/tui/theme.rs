//! Colour and style tokens with a fixed meaning.
//!
//! Every view styles itself from this module instead of picking colours ad
//! hoc, so the same idea always reads the same way: `ok`/`warn`/`error` for
//! state, `chrome` for structure the user should not read closely, `action`
//! for what can be taken, `active` for what is selected right now, and
//! `content` for the text itself.

use ratatui::style::{Color, Modifier, Style};

/// Successful state.
pub fn ok() -> Style {
    Style::default().fg(Color::Green)
}

/// State that needs attention but is not a failure.
pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Failed state.
pub fn error() -> Style {
    Style::default().fg(Color::Red)
}

/// Structural UI: separators, hints, labels. Recedes behind the content.
pub fn chrome() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Something the user can act on: a shortcut, a selectable field.
pub fn action() -> Style {
    Style::default().fg(Color::Cyan)
}

/// The current selection. Bold is the only bold in the interface, so "bold"
/// and "selected" mean the same thing everywhere.
pub fn active() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Ordinary body text.
pub fn content() -> Style {
    Style::default().fg(Color::White)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn tokens_have_their_documented_colours() {
        assert_eq!(ok().fg, Some(Color::Green));
        assert_eq!(warn().fg, Some(Color::Yellow));
        assert_eq!(error().fg, Some(Color::Red));
        assert_eq!(chrome().fg, Some(Color::DarkGray));
        assert_eq!(action().fg, Some(Color::Cyan));
        assert_eq!(active().fg, Some(Color::Yellow));
        assert_eq!(content().fg, Some(Color::White));
    }

    #[test]
    fn only_active_is_bold() {
        assert!(active().add_modifier.contains(Modifier::BOLD));
        assert!(!content().add_modifier.contains(Modifier::BOLD));
    }
}
