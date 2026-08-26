//! A bandwidth sparkline.
//!
//! Drawn with Cairo rather than assembled from widgets: a chart is a hundred
//! small rectangles, and a hundred widgets updated twice a second is how a
//! GTK application starts dropping frames.
//!
//! Colours come from the widget's own style context, so the graph follows the
//! theme and the user's accent colour instead of hard-coding a palette.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;

use super::format::human_bytes;
use crate::{adw, gtk};

/// How many samples the graph keeps. At two per second this is about a minute.
const SAMPLES: usize = 120;
const HEIGHT: i32 = 56;

/// A rolling record of download and upload speed.
pub struct Bandwidth {
    area: gtk::DrawingArea,
    root: gtk::Box,
    caption: gtk::Label,
    samples: Rc<RefCell<VecDeque<(u64, u64)>>>,
}

impl Bandwidth {
    pub fn new() -> Self {
        let samples: Rc<RefCell<VecDeque<(u64, u64)>>> =
            Rc::new(RefCell::new(VecDeque::with_capacity(SAMPLES)));

        let area = gtk::DrawingArea::builder()
            .content_height(HEIGHT)
            .hexpand(true)
            // The class goes on the drawing area itself: the draw function
            // reads this widget's `color`, and a descendant selector on the
            // parent does not set it.
            .css_classes(["snatch-graph-area"])
            .build();

        area.set_draw_func({
            let samples = Rc::clone(&samples);
            move |area, context, width, height| {
                draw(area, context, width, height, &samples.borrow());
            }
        });

        let caption = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["snatch-detail"])
            .label("Idle")
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_bottom(12)
            .css_classes(["snatch-graph"])
            .visible(false)
            .build();
        root.append(&caption);
        root.append(&area);

        Self {
            area,
            root,
            caption,
            samples,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Record one sample and redraw.
    pub fn push(&self, down: u64, up: u64) {
        {
            let mut samples = self.samples.borrow_mut();
            if samples.len() == SAMPLES {
                samples.pop_front();
            }
            samples.push_back((down, up));
        }

        let (peak, latest_down, latest_up) = {
            let samples = self.samples.borrow();
            let peak = samples
                .iter()
                .map(|(down, up)| (*down).max(*up))
                .max()
                .unwrap_or(0);
            (peak, down, up)
        };

        // Hide the graph entirely while nothing has ever moved, rather than
        // showing a flat line that says nothing.
        let has_traffic = peak > 0;
        self.root.set_visible(has_traffic);
        if !has_traffic {
            return;
        }

        let mut caption = format!("↓ {}/s", human_bytes(latest_down));
        if latest_up > 0 {
            caption.push_str(&format!("   ↑ {}/s", human_bytes(latest_up)));
        }
        caption.push_str(&format!("   peak {}/s", human_bytes(peak)));
        self.caption.set_text(&caption);
        self.area.queue_draw();
    }
}

fn draw(
    area: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: i32,
    height: i32,
    samples: &VecDeque<(u64, u64)>,
) {
    if samples.is_empty() || width <= 0 || height <= 0 {
        return;
    }

    let width = f64::from(width);
    let height = f64::from(height);
    let peak = samples
        .iter()
        .map(|(down, up)| (*down).max(*up))
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    // The colour comes from the widget's `color`, which style.css binds to the
    // theme accent. Reading it here rather than hard-coding one means a custom
    // accent and a dark theme both work.
    let colour = area.color();
    // Spread the samples we actually have across the whole width. Scaling to
    // the full buffer instead would draw a thumbnail in one corner until the
    // buffer filled, which is what it did before.
    let span = samples.len().saturating_sub(1).max(1) as f64;
    let step = width / span;
    let offset = 0.0;

    // Download: filled area.
    context.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        0.22,
    );
    context.move_to(offset, height);
    for (index, (down, _)) in samples.iter().enumerate() {
        let x = offset + step * index as f64;
        let y = height - (*down as f64 / peak) * (height - 2.0);
        context.line_to(x, y);
    }
    context.line_to(
        offset + step * (samples.len().saturating_sub(1)) as f64,
        height,
    );
    context.close_path();
    let _ = context.fill();

    // Download: the line on top.
    context.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        0.85,
    );
    context.set_line_width(1.5);
    for (index, (down, _)) in samples.iter().enumerate() {
        let x = offset + step * index as f64;
        let y = height - (*down as f64 / peak) * (height - 2.0);
        if index == 0 {
            context.move_to(x, y);
        } else {
            context.line_to(x, y);
        }
    }
    let _ = context.stroke();

    // Upload: a dashed line, only when there is any.
    if samples.iter().any(|(_, up)| *up > 0) {
        context.set_dash(&[3.0, 3.0], 0.0);
        context.set_line_width(1.0);
        context.set_source_rgba(
            f64::from(colour.red()),
            f64::from(colour.green()),
            f64::from(colour.blue()),
            0.5,
        );
        for (index, (_, up)) in samples.iter().enumerate() {
            let x = offset + step * index as f64;
            let y = height - (*up as f64 / peak) * (height - 2.0);
            if index == 0 {
                context.move_to(x, y);
            } else {
                context.line_to(x, y);
            }
        }
        let _ = context.stroke();
        context.set_dash(&[], 0.0);
    }
}
