//! Formatting helpers shared by every page.

use crate::gtk;
use gtk::prelude::*;

/// Binary units, because that is what a download manager's peers report.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    let name = UNITS[unit];
    if value >= 100.0 {
        format!("{value:.0} {name}")
    } else if value >= 10.0 {
        format!("{value:.1} {name}")
    } else {
        format!("{value:.2} {name}")
    }
}

pub fn human_duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Shorten a string for a header or a toast, without splitting a character.
pub fn elide(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A flat circular icon button, the standard control in every row.
pub fn control_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build()
}

pub fn heading_label() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["heading"])
        .build()
}

pub fn caption_label(align_end: bool) -> gtk::Label {
    gtk::Label::builder()
        .xalign(if align_end { 1.0 } else { 0.0 })
        .hexpand(!align_end)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["caption", "dim-label"])
        .build()
}

/// The vertical box used as the body of every list row.
pub fn row_body() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Content inside a clamp and a scroller: the standard page body.
pub fn boxed_list_page(content: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw_clamp(content);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build()
}

fn adw_clamp(child: &impl IsA<gtk::Widget>) -> libadwaita::Clamp {
    libadwaita::Clamp::builder()
        .maximum_size(920)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .child(child)
        .build()
}

pub fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

pub fn plain_row(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(child)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_scale_and_keep_useful_precision() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(104_857_600), "100 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 15), "15.0 MiB");
    }

    #[test]
    fn elide_never_splits_a_character() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("abcdefghij", 5), "abcd…");
        // Multi-byte input must be cut on a character, not a byte.
        assert_eq!(elide("ααααααα", 4), "ααα…");
    }

    #[test]
    fn durations_drop_units_that_are_zero() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(125), "2m 05s");
        assert_eq!(human_duration(7325), "2h 02m");
    }
}
