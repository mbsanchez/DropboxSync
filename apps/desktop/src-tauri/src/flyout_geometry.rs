//! Where the tray flyout goes (DBSYNC-85).
//!
//! Split out of `position_flyout` in `lib.rs` so the placement math takes the **monitor list
//! as an input** instead of looking it up. That is not tidiness: the development machine has
//! one display, and the bug being chased only appears with two. Once a layout is four numbers
//! in a parameter, the arrangements nobody here owns become ordinary test data.
//!
//! **What this module deliberately does NOT do** is correct the reported multi-monitor
//! inversion. The suspicion is that `tray-icon` reports clicks in Cocoa coordinates (origin
//! bottom-left) while `monitor_from_point` expects winit's top-left space — but that is
//! unmeasured, and a coordinate flip has roughly a coin-flip chance of looking correct on one
//! arrangement while being wrong on another. Measurement is GitHub #112, the fix is #113.

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
