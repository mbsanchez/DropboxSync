//! Where the tray flyout goes (DBSYNC-85).
//!
//! Split out of `position_flyout` in `lib.rs` so the placement math takes the **monitor list
//! as an input** instead of looking it up. That is not tidiness: the development machine has
//! one display, and the bug being chased only appears with two. Once a layout is four numbers
//! in a parameter, the arrangements nobody here owns become ordinary test data.
//!
//! **The inversion turned out not to be a coordinate flip.** The original suspicion was that
//! `tray-icon` reports clicks in Cocoa coordinates (origin bottom-left) while the monitor lookup
//! expects winit's top-left space. Measured on 2026-08-22 with both displays connected (GitHub
//! #112): it does not. Both are the same top-left-origin, physical-pixel global space, and each
//! click landed ~35px below the top edge of *its own* display. No sign was ever flipped.
//!
//! What actually resolved the symptom was replacing `AppHandle::monitor_from_point` with the
//! plain containment test in [`monitor_for_point`] — so on this project, do not reach back for
//! that lookup. `maintainer_layout` in the tests below pins the arrangement that was broken.

/// A display, as far as placement is concerned. Physical pixels, top-left origin — the space
/// Tauri's `Monitor` reports in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MonitorBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl MonitorBox {
    fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// The tray icon's rectangle, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TrayRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Picks the display a click at `(px, py)` landed on.
///
/// Falls back to the first entry — the caller passes the primary first — when the point is
/// outside every display. That happens more often than it sounds: it is exactly what a
/// coordinate-space mismatch produces, and returning `None` here would leave the window
/// unplaced rather than merely misplaced.
pub(crate) fn monitor_for_point(monitors: &[MonitorBox], px: f64, py: f64) -> Option<MonitorBox> {
    monitors
        .iter()
        .find(|m| m.contains(px, py))
        .or_else(|| monitors.first())
        .copied()
}

/// The flyout's top-left corner, clamped to stay wholly inside `monitor`.
///
/// Right-aligned to the tray icon, and **below** it on macOS: the menu bar is at the top of the
/// screen, so a window anchored above it would be off-screen. The previous code subtracted the
/// window height — a bottom-taskbar assumption inherited from Windows — and the clamp then
/// pushed the result back to the top of the display, which made it look correct by accident on
/// a single monitor. Windows keeps the original behaviour: its taskbar really is at the bottom.
pub(crate) fn flyout_origin(
    tray: TrayRect,
    monitor: MonitorBox,
    win_w: f64,
    win_h: f64,
    gap: f64,
) -> (f64, f64) {
    let x = tray.x + tray.width - win_w;

    #[cfg(target_os = "macos")]
    let y = tray.y + tray.height + gap;
    #[cfg(not(target_os = "macos"))]
    let y = tray.y - win_h - gap;

    clamp_into(x, y, monitor, win_w, win_h)
}

/// Keeps the window inside the display.
///
/// When the window is larger than the display, `max < min` and `clamp` would panic, so that
/// case pins to the origin instead. A window wider than the screen is a bad look; a panic in
/// the click handler is a crash.
fn clamp_into(x: f64, y: f64, m: MonitorBox, win_w: f64, win_h: f64) -> (f64, f64) {
    let min_x = m.x;
    let max_x = m.x + m.width - win_w;
    let min_y = m.y;
    let max_y = m.y + m.height - win_h;

    let cx = if max_x >= min_x { x.clamp(min_x, max_x) } else { min_x };
    let cy = if max_y >= min_y { y.clamp(min_y, max_y) } else { min_y };
    (cx, cy)
}

#[cfg(test)]
mod tests {
    use super::{flyout_origin, monitor_for_point, MonitorBox, TrayRect};

    // Two 1440x900@2x displays side by side, primary on the left. Physical pixels.
    fn side_by_side() -> Vec<MonitorBox> {
        vec![
            MonitorBox { x: 0.0, y: 0.0, width: 2880.0, height: 1800.0 },
            MonitorBox { x: 2880.0, y: 0.0, width: 2880.0, height: 1800.0 },
        ]
    }

    // The same two, stacked with the secondary ABOVE the primary — the arrangement under which
    // a Y-axis flip would map each menu bar onto the other screen. The layout is data, so this
    // is testable on a machine with one display.
    fn stacked() -> Vec<MonitorBox> {
        vec![
            MonitorBox { x: 0.0, y: 0.0, width: 2880.0, height: 1800.0 },
            MonitorBox { x: 0.0, y: -1800.0, width: 2880.0, height: 1800.0 },
        ]
    }

    #[test]
    fn a_click_on_the_primary_picks_the_primary() {
        let m = monitor_for_point(&side_by_side(), 100.0, 20.0).unwrap();
        assert_eq!(m.x, 0.0);
    }

    #[test]
    fn a_click_on_the_second_screen_picks_the_second_screen() {
        let m = monitor_for_point(&side_by_side(), 3000.0, 20.0).unwrap();
        assert_eq!(m.x, 2880.0);
    }

    #[test]
    fn a_click_on_a_screen_above_the_primary_picks_that_screen() {
        let m = monitor_for_point(&stacked(), 100.0, -1700.0).unwrap();
        assert_eq!(m.y, -1800.0);
    }

    /// A point outside every display is what a coordinate-space mismatch produces. Falling back
    /// beats returning nothing: the window ends up misplaced rather than never shown.
    #[test]
    fn a_point_outside_every_display_falls_back_to_the_first() {
        let m = monitor_for_point(&side_by_side(), 99_999.0, 99_999.0).unwrap();
        assert_eq!(m.x, 0.0);
    }

    #[test]
    fn no_monitors_yields_none() {
        assert!(monitor_for_point(&[], 0.0, 0.0).is_none());
    }

    /// DBSYNC-85's second defect. On macOS the menu bar is at the TOP, so the flyout belongs
    /// below the icon. The old code subtracted the window height and relied on the clamp to
    /// drag it back — correct-looking on one display, wrong in principle.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hangs_the_flyout_below_the_menu_bar() {
        let tray = TrayRect { x: 1400.0, y: 0.0, width: 44.0, height: 48.0 };
        let m = side_by_side()[0];
        let (_, y) = flyout_origin(tray, m, 720.0, 1200.0, 16.0);
        assert_eq!(y, 64.0, "below the 48px tray rect plus the 16px gap");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_keep_the_bottom_taskbar_anchor() {
        let tray = TrayRect { x: 1400.0, y: 1752.0, width: 44.0, height: 48.0 };
        let m = side_by_side()[0];
        let (_, y) = flyout_origin(tray, m, 720.0, 1200.0, 16.0);
        assert!(y < 1752.0, "the window sits above a bottom taskbar");
    }

    #[test]
    fn the_flyout_is_right_aligned_to_the_icon() {
        let tray = TrayRect { x: 1400.0, y: 0.0, width: 44.0, height: 48.0 };
        let m = side_by_side()[0];
        let (x, _) = flyout_origin(tray, m, 720.0, 1200.0, 16.0);
        assert_eq!(x, 1400.0 + 44.0 - 720.0);
    }

    /// The icon sits near the right edge of the SECOND screen. The window must stay on that
    /// screen — the clamp is relative to the chosen monitor, not to the desktop origin.
    #[test]
    fn clamping_keeps_the_window_on_the_second_screen() {
        let monitors = side_by_side();
        let tray = TrayRect { x: 5700.0, y: 0.0, width: 44.0, height: 48.0 };
        let m = monitor_for_point(&monitors, 5720.0, 20.0).unwrap();
        let (x, _) = flyout_origin(tray, m, 720.0, 1200.0, 16.0);
        assert!(x >= 2880.0, "must not spill onto the primary");
        assert!(x + 720.0 <= 5760.0, "must not run off the right edge");
    }

    /// A display smaller than the window makes `max < min`; `f64::clamp` panics on that.
    /// A crash in the click handler is worse than an awkwardly placed window.
    #[test]
    fn a_window_larger_than_the_display_does_not_panic() {
        let tiny = MonitorBox { x: 0.0, y: 0.0, width: 400.0, height: 300.0 };
        let tray = TrayRect { x: 300.0, y: 0.0, width: 44.0, height: 48.0 };
        let (x, y) = flyout_origin(tray, tiny, 720.0, 1200.0, 16.0);
        assert_eq!((x, y), (0.0, 0.0));
    }

    // ---- Characterization: the maintainer's real arrangement (DBSYNC-85 Slice 3, #113) ----
    //
    // Characterization, NOT regression: these pin what the shipped code does on the hardware the
    // bug was reported on. They do not guard the Y axis, and the doc comments below say why.

    /// The arrangement that actually produced the reported inversion, measured 2026-08-22.
    ///
    /// Physical pixels, taken verbatim from the `dbsync85` log line (GitHub #112). The built-in
    /// panel is the primary at the origin; the external display sits to its **left** and slightly
    /// **above**, which is why both of its coordinates are negative. Every other test in this
    /// module uses invented layouts — this one is the hardware the bug was reported on.
    fn maintainer_layout() -> Vec<MonitorBox> {
        vec![
            MonitorBox { x: 0.0, y: 0.0, width: 3360.0, height: 2100.0 },
            MonitorBox { x: -6016.0, y: -1076.0, width: 6016.0, height: 3384.0 },
        ]
    }

    /// Both clicks verbatim from the log — not rounded, not reconstructed.
    const CLICK_ON_BUILT_IN: (f64, f64) = (1898.28125, 34.1484375);
    const CLICK_ON_EXTERNAL: (f64, f64) = (-1456.9453125, -1040.0);

    /// Each measured click falls inside its own display and outside the other.
    ///
    /// This exercises [`MonitorBox::contains`], the predicate the whole lookup is built on, and it
    /// fails if that predicate breaks — verified by mutation, not assumed.
    ///
    /// **What it does not do is guard the Y axis**, and it would be dishonest to imply otherwise:
    /// the two displays have zero horizontal overlap, so X alone separates them. A sign flip on Y
    /// leaves this green. The layouts that catch one are the invented stacked fixtures above,
    /// which share an X range.
    ///
    /// The offsets below are recorded measurement, not a check that can fail — 34px on the
    /// built-in (top edge 0), 36px on the external (top edge -1076). They are the evidence that Y
    /// grows downward from one shared origin, kept here beside the data they describe.
    #[test]
    fn each_measured_click_falls_inside_its_own_display_and_no_other() {
        let layout = maintainer_layout();
        let built_in = layout[0];
        let external = layout[1];

        assert!(
            built_in.contains(CLICK_ON_BUILT_IN.0, CLICK_ON_BUILT_IN.1),
            "the built-in click must be inside the built-in panel"
        );
        assert!(
            !external.contains(CLICK_ON_BUILT_IN.0, CLICK_ON_BUILT_IN.1),
            "the built-in click must NOT also be inside the external display"
        );

        assert!(
            external.contains(CLICK_ON_EXTERNAL.0, CLICK_ON_EXTERNAL.1),
            "the external click must be inside the external display"
        );
        assert!(
            !built_in.contains(CLICK_ON_EXTERNAL.0, CLICK_ON_EXTERNAL.1),
            "the external click must NOT also be inside the built-in panel"
        );
    }

    /// Click a screen, get that screen — both directions, against the real layout.
    ///
    /// **This is a characterization test, not a regression test, and the difference matters.**
    /// Mutation-tested while writing it: flipping the sign of `py` in [`monitor_for_point`] — the
    /// exact "fix" this ticket forbade guessing — leaves it **green**. The two displays have zero
    /// horizontal overlap, so X alone identifies them and Y never gets a vote; the built-in click
    /// then resolves through the fallback and still arrives at the right answer.
    ///
    /// So the maintainer's own arrangement cannot detect a Y-axis defect at all. What catches one
    /// is `stacked()` above, where the displays share an X range — under the same mutation those
    /// tests go red. Both kinds are kept deliberately: this one pins what the hardware does, that
    /// one has the teeth.
    #[test]
    fn each_measured_click_resolves_to_the_display_it_was_made_on() {
        let layout = maintainer_layout();

        let m = monitor_for_point(&layout, CLICK_ON_BUILT_IN.0, CLICK_ON_BUILT_IN.1).unwrap();
        assert_eq!((m.x, m.y), (0.0, 0.0), "click on the built-in must pick the built-in");

        let m = monitor_for_point(&layout, CLICK_ON_EXTERNAL.0, CLICK_ON_EXTERNAL.1).unwrap();
        assert_eq!(
            (m.x, m.y),
            (-6016.0, -1076.0),
            "click on the external must pick the external"
        );
    }

    /// And the window itself stays wholly on the clicked display, both ways.
    ///
    /// Same caveat as the test above: green under a `py` sign flip, for the same reason. It pins
    /// behaviour on the real hardware; it does not guard the Y axis.
    ///
    /// The tray *rect* was not captured — the Slice 1 instrumentation logs the click position and
    /// the monitor list but emits the line before the rect is converted (noted on #112). The rects
    /// below are therefore reconstructed from each click plus a standard Retina menu-bar height,
    /// not measured. The assertion is containment, which does not turn on their exact values.
    #[test]
    fn the_flyout_stays_on_the_clicked_display_in_both_directions() {
        let layout = maintainer_layout();
        let (win_w, win_h, gap) = (720.0, 1200.0, 16.0);

        for (click, expected) in [
            (CLICK_ON_BUILT_IN, layout[0]),
            (CLICK_ON_EXTERNAL, layout[1]),
        ] {
            let monitor = monitor_for_point(&layout, click.0, click.1).unwrap();
            assert_eq!((monitor.x, monitor.y), (expected.x, expected.y));

            let tray = TrayRect { x: click.0, y: expected.y, width: 44.0, height: 48.0 };
            let (x, y) = flyout_origin(tray, monitor, win_w, win_h, gap);

            assert!(
                x >= monitor.x && x + win_w <= monitor.x + monitor.width,
                "window spilled horizontally off the clicked display: x={x}"
            );
            assert!(
                y >= monitor.y && y + win_h <= monitor.y + monitor.height,
                "window spilled vertically off the clicked display: y={y}"
            );
        }
    }

    /// Scale factors differ between a Retina laptop and an external display, so the numbers
    /// arrive already in physical pixels and the function must not assume they match.
    #[test]
    fn a_non_retina_second_display_is_handled_by_its_own_geometry() {
        let monitors = vec![
            MonitorBox { x: 0.0, y: 0.0, width: 2880.0, height: 1800.0 },
            MonitorBox { x: 2880.0, y: 0.0, width: 1920.0, height: 1080.0 },
        ];
        let m = monitor_for_point(&monitors, 3000.0, 10.0).unwrap();
        let tray = TrayRect { x: 4700.0, y: 0.0, width: 22.0, height: 24.0 };
        let (x, _) = flyout_origin(tray, m, 360.0, 600.0, 8.0);
        assert!(x >= 2880.0 && x + 360.0 <= 4800.0);
    }
}
