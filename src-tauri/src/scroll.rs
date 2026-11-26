//! Manual-scroll long screenshot: the user picks a region, scrolls the page
//! themselves, and this module keeps capturing the region and stitching every
//! new row into one tall image.
//!
//! Why manual scrolling: driving the target app with synthetic wheel events
//! needs the Accessibility permission, and several apps ignore such events
//! anyway. Positional scrolling works everywhere, so the only thing we do is
//! watch for new content every `TICK` milliseconds and extend the canvas.
//!
//! Threads: one background thread captures (ScreenCaptureKit calls must stay
//! off the main thread, same as `capture_all`); window and AppKit work hops to
//! the main thread via `run_on_main_thread`. The job lives in
//! `AppState.scroll`, guarded by its mutex; `stop` ends the loop from anywhere.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior, NSScreenSaverWindowLevel};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::capture::{self, IRect};
use crate::clog;
use crate::geo::{MonitorInfo, Rect};
use crate::util::{file_stamp, lock, now_ms};
use crate::AppState;

pub const OUTLINE_LABEL: &str = "scroll-outline";
pub const PANEL_LABEL: &str = "scroll-panel";
/// The self test's auto-scrolling driver window (see `ensure_driver_window`).
pub const DRIVER_LABEL: &str = "scroll-driver";

/// How often to look for new content. Fast enough that a normal wheel/trackpad
/// scroll never outruns the overlap the stitcher needs, and short enough that
/// a scrolling page with a fixed rhythm (every ~1 s) can never stay
/// phase-locked with the ticks: consecutive identical frames — the settle
/// trigger — then appear within a few hundred milliseconds of any pause.
/// (ScreenCaptureKit state is cached per session, so a tick is cheap.)
const TICK: Duration = Duration::from_millis(150);
/// PNG height cap: beyond this, tools and chat apps start choking on the image.
const MAX_HEIGHT_PX: usize = 20_000;
/// Consecutive failed captures before we give up (the display went away, or
/// the session was invalidated system-wide).
const MAX_MISSES: u32 = 40;
/// A scroll session that has found no new content for this long ends itself,
/// so a forgotten panel doesn't capture the screen forever.
const IDLE_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// Width of the live preview thumbnail in the control panel.
const PREVIEW_WIDTH: u32 = 160;
/// Minimum gap between panel preview updates.
const PREVIEW_THROTTLE_MS: u64 = 500;
const PANEL_SIZE: (f64, f64) = (302.0, 122.0);

// ------------------------------------------------------------------ stitching

/// The growing image: `rgba` is top-down with stride `width * 4`.
pub struct Stitched {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Stitched {
    /// Append the new rows of `frame` (height `frame_h`) below the canvas.
    /// `offset` is how far the content scrolled down, i.e. frame rows
    /// `[height - offset .. frame_h)` are the new content.
    fn append(&mut self, frame: &[u8], frame_h: usize, offset: usize) {
        let w = self.width;
        let old_h = self.height;
        let new_rows = frame_h + offset - old_h;
        self.rgba.resize((old_h + new_rows) * w * 4, 0);
        let src = (old_h - offset) * w * 4;
        let dst = old_h * w * 4;
        self.rgba[dst..].copy_from_slice(&frame[src..]);
        self.height = old_h + new_rows;
    }
}

pub mod stitch {
    //! Pure pixel matching, unit-tested without ScreenCaptureKit.

    /// Rows of the reference strip taken from the canvas. Long enough to be
    /// distinctive, short enough to survive a fast scroll.
    const STRIP_MAX: usize = 24;
    /// Column subsampling: every Nth physical pixel participates.
    const COL_STEP: usize = 4;
    /// A candidate counts as a match below this mean absolute difference
    /// (0–255 grayscale units).
    const MEAN_DIFF_MAX: i64 = 10;
    /// A strip flatter than this (a UI row's solid padding) matches anywhere
    /// and is refused — see `scroll_offset` for how we walk around it.
    const FLAT_DIFF: i64 = 2;
    /// Row-step while walking up for a textured strip.
    const WALK_STEP: usize = 16;
    /// How far up the canvas the walk may go.
    const WALK_LIMIT: usize = 400;
    /// Gap between the primary and the verification strip (one UI row).
    const VERIFY_OFFSET: usize = 160;

    /// Where does `frame` continue `stitched`?
    ///
    /// Returns how far the content scrolled down (`d`): frame row `i` shows
    /// canvas row `d + i`, so `frame[height - d ..]` is the new content to
    /// append (empty when `d + frame_h <= height`, i.e. everything was already
    /// on the canvas). `None` means no confident match — the user scrolled
    /// more than a frame in one tick, or the content jumped.
    ///
    /// Two real-world quirks shape the algorithm:
    ///
    /// * macOS settles a scroll gesture at *fractional* pixels, where every
    ///   rendered row blends its neighbours and a row-exact comparison misses
    ///   by one row everywhere. Rows are therefore compared in pairs
    ///   (vertically box-blurred), with the blur grid moving with each
    ///   candidate so the search keeps 1 px granularity.
    /// * The canvas's bottom rows are often *solid padding* — list rows and
    ///   headings carry a text band and then rows of plain background, and a
    ///   solid strip would match anywhere. The strip therefore starts at the
    ///   canvas bottom and walks upward until it contains texture, and the
    ///   offset maths generalizes to a strip anchored at `y0` instead of the
    ///   exact bottom.

    /// `scroll_offset_scored` plus the best candidate's mean difference, for
    /// diagnostics: a value near the gate means "almost matched", a huge one
    /// means the pixels don't line up at all (wrong geometry, or the content
    /// never scrolled between the two frames).
    pub fn scroll_offset_scored(
        stitched: &[u8],
        stitched_h: usize,
        frame: &[u8],
        frame_h: usize,
        width: usize,
    ) -> (Option<usize>, f64) {
        let strip = STRIP_MAX.min(stitched_h).min(frame_h) & !1; // even: row pairs
        if strip < 4 || width == 0 {
            return (None, f64::INFINITY);
        }
        let pairs = strip / 2;
        let cols: Vec<usize> = (0..width).step_by(COL_STEP).collect();
        let gray = |px: &[u8]| (px[0] as i64 * 3 + px[1] as i64 * 4 + px[2] as i64) / 8;
        // A pair-blurred sample: the average of rows `row` and `row + 1`.
        let blurred = |buf: &[u8], row: usize, col: usize| -> i64 {
            (gray(&buf[(row * width + col) * 4..]) + gray(&buf[((row + 1) * width + col) * 4..])) / 2
        };
        let samples = (pairs * cols.len()) as i64;
        let gate = MEAN_DIFF_MAX * samples;

        // Walk the strip anchor up from the bottom until it holds texture.
        let mut best_overall = f64::INFINITY;
        let mut y0 = stitched_h.saturating_sub(strip);
        let floor = stitched_h.saturating_sub(WALK_LIMIT).min(y0);
        loop {
            // The strip at anchor `y0`, pair-blurred, downsampled to columns.
            let mut strip_px = vec![0i64; pairs * cols.len()];
            for (k, row) in strip_px.chunks_mut(cols.len()).enumerate() {
                let base = y0 + k * 2;
                for (c, col) in cols.iter().enumerate() {
                    row[c] = blurred(stitched, base, *col);
                }
            }
            // A featureless strip (a UI row's padding) matches anywhere.
            let mean = strip_px.iter().sum::<i64>() / samples;
            let flat = strip_px.iter().all(|&v| (v - mean).abs() < FLAT_DIFF);

            let mut best_sad = i64::MAX;
            let mut best: Option<usize> = None;
            if !flat {
                // Locate the strip in the frame. Candidates walk every
                // original row; the blur grid moves with the candidate, so a
                // pair at candidate `s` covers frame rows `s + 2k .. s + 2k + 2`.
                // `s` aligning with anchor `y0` means the content scrolled
                // `y0 - s` pixels (which must be positive to add anything).
                for s in 0..=(frame_h.saturating_sub(strip)) {
                    // d == 0 ("frame is the canvas's continuation, nothing
                    // new") must stay a candidate: the caller filters it, and
                    // excluding it here would let periodic content win with a
                    // wrong, non-zero offset on identical frames.
                    let Some(d) = y0.checked_sub(s) else { continue };
                    let mut sad = 0i64;
                    for (k, row) in strip_px.chunks(cols.len()).enumerate() {
                        let base = s + k * 2;
                        for (c, col) in cols.iter().enumerate() {
                            sad += (blurred(frame, base, *col) - row[c]).abs();
                        }
                        // Already worse than the best candidate or beyond the
                        // gate: no point scanning the rest of this candidate.
                        if sad >= best_sad.min(gate) {
                            break;
                        }
                    }
                    if sad < best_sad {
                        best_sad = sad;
                        best = Some(d);
                    }
                }
            }
            if best_sad < i64::MAX {
                best_overall = best_overall.min(best_sad as f64 / samples as f64);
            }
            if best_sad < gate {
                // Two-strip verification: a second anchor one UI row above
                // must agree on the same offset. UI content is quasi-periodic
                // (identical list rows, striped backgrounds), and appending on
                // an aliasing match is exactly what reads as a "deformed"
                // long screenshot — so a failed cross-check drops the frame.
                let Some(d) = best else { return (None, best_overall) };
                let s = y0 - d;
                let verified = s < VERIFY_OFFSET || {
                    let y0v = y0 - VERIFY_OFFSET;
                    let sv = s - VERIFY_OFFSET;
                    let mut vs = vec![0i64; pairs * cols.len()];
                    for (k, row) in vs.chunks_mut(cols.len()).enumerate() {
                        let base = y0v + k * 2;
                        for (c, col) in cols.iter().enumerate() {
                            row[c] = blurred(stitched, base, *col);
                        }
                    }
                    let vmean = vs.iter().sum::<i64>() / samples;
                    let vflat = vs.iter().all(|&v| (v - vmean).abs() < FLAT_DIFF);
                    if vflat {
                        true // nothing to check against; accept the primary
                    } else {
                        let mut vsad = 0i64;
                        for (k, row) in vs.chunks(cols.len()).enumerate() {
                            let base = sv + k * 2;
                            for (c, col) in cols.iter().enumerate() {
                                vsad += (blurred(frame, base, *col) - row[c]).abs();
                            }
                        }
                        vsad < gate
                    }
                };
                if verified {
                    return (best, best_sad as f64 / samples as f64);
                }
                // Aliasing match: stop, don't trust anything at this anchor.
                return (None, best_sad as f64 / samples as f64);
            }
            if y0 <= floor {
                return (None, best_overall);
            }
            y0 = y0.saturating_sub(WALK_STEP).max(floor);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::scroll::Stitched;

        fn scroll_offset(stitched: &[u8], sh: usize, frame: &[u8], fh: usize, w: usize) -> Option<usize> {
            scroll_offset_scored(stitched, sh, frame, fh, w).0
        }

        const W: usize = 40;
        /// Tall enough that the 24-row reference strip stays visible in a
        /// frame scrolled by up to ~20 px (`H - STRIP_MAX > d`).
        const H: usize = 60;

        /// Deterministic textured content: `page(x, y)` for any canvas row.
        fn page(x: usize, y: usize) -> [u8; 4] {
            let v = |k: usize| ((x * 31 + y * 17 + k * 57) % 251) as u8;
            [v(0), v(1), v(2), 255]
        }

        /// A frame showing canvas rows `[d .. d + H)`, with the bottom `d`
        /// rows replaced by flat filler (new content the canvas never saw).
        fn frame_at(d: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(W * H * 4);
            for y in 0..H {
                for x in 0..W {
                    let px = if y + d >= H { [250, 250, 250, 255] } else { page(x, y + d) };
                    out.extend_from_slice(&px);
                }
            }
            out
        }

        fn canvas() -> Vec<u8> {
            let mut out = Vec::with_capacity(W * H * 4);
            for y in 0..H {
                for x in 0..W {
                    out.extend_from_slice(&page(x, y));
                }
            }
            out
        }

        #[test]
        fn detects_the_scrolled_distance() {
            for d in [1usize, 3, 7, 15] {
                let frame = frame_at(d);
                assert_eq!(scroll_offset(&canvas(), H, &frame, H, W), Some(d), "scroll {d}");
            }
        }

        #[test]
        fn identical_frames_mean_no_scroll() {
            assert_eq!(scroll_offset(&canvas(), H, &canvas(), H, W), Some(0));
        }

        /// A frame rendered while a smooth-scroll animation settles: the
        /// content stops between pixel rows, so every rendered row blends its
        /// neighbours (`3:1` here). On smooth content the pair-blurred
        /// comparison must still find the offset.
        #[test]
        fn half_pixel_settle_still_matches() {
            // Smooth content: blends of neighbouring rows stay close to the
            // rows themselves, like real UI.
            let soft = |x: usize, y: usize| -> [u8; 4] {
                let v = ((x * 3 + y * 5 + 11) % 199) as u8;
                let g = ((v as usize * 3) % 199) as u8;
                let b = ((v as usize * 7) % 199) as u8;
                [v, g, b, 255]
            };
            let canvas_of = |f: &dyn Fn(usize, usize) -> [u8; 4]| {
                let mut out = Vec::with_capacity(W * H * 4);
                for y in 0..H {
                    for x in 0..W {
                        out.extend_from_slice(&f(x, y));
                    }
                }
                out
            };
            let stitched = canvas_of(&|x, y| soft(x, y));
            let d = 8;
            let mut frame = Vec::with_capacity(W * H * 4);
            for y in 0..H {
                for x in 0..W {
                    let a = soft(x, y + d);
                    let b = soft(x, y + d + 1);
                    frame.extend_from_slice(&[
                        (a[0] as u16 * 3 / 4 + b[0] as u16 / 4) as u8,
                        (a[1] as u16 * 3 / 4 + b[1] as u16 / 4) as u8,
                        (a[2] as u16 * 3 / 4 + b[2] as u16 / 4) as u8,
                        255,
                    ]);
                }
            }
            assert_eq!(scroll_offset(&stitched, H, &frame, H, W), Some(d), "half-pixel settle");
        }

        /// List-style UI: each row is a text band over long solid padding, so
        /// the canvas's bottom strip lands in flat padding. The matcher must
        /// walk up to the textured band instead of giving up (this exact case
        /// ended real sessions with zero growth).
        #[test]
        fn flat_bottom_walks_up_to_texture() {
            // Page rows 0..36 textured, 36.. solid padding.
            let page = |x: usize, y: usize| -> [u8; 4] {
                if y >= 36 {
                    [250, 250, 250, 255]
                } else {
                    // Same texture as `page` above, spelled out because a
                    // closure can't recurse.
                    let v = |k: usize| ((x * 31 + y * 17 + k * 57) % 251) as u8;
                    [v(0), v(1), v(2), 255]
                }
            };
            let canvas_of = |off: usize| {
                let mut out = Vec::with_capacity(W * H * 4);
                for y in off..off + H {
                    for x in 0..W {
                        out.extend_from_slice(&page(x, y));
                    }
                }
                out
            };
            let stitched = canvas_of(0);
            for d in [2usize, 6, 12] {
                let frame = canvas_of(d);
                assert_eq!(scroll_offset(&stitched, H, &frame, H, W), Some(d), "flat bottom, scroll {d}");
            }
        }

        #[test]
        fn flat_strips_are_refused() {
            // A uniform canvas: every strip is featureless, so no offset is
            // trustworthy in the first place.
            let flat = vec![128u8; W * H * 4];
            assert_eq!(scroll_offset(&flat, H, &flat, H, W), None);
        }

        #[test]
        fn unrelated_content_does_not_match() {
            // Every row differs from the canvas: nothing lines up.
            let other: Vec<u8> = (0..W * H)
                .flat_map(|i| {
                    let i = i as u32;
                    [(i * 7 % 251) as u8, (i * 13 % 249) as u8, (i * 29 % 243) as u8, 255]
                })
                .collect();
            assert_eq!(scroll_offset(&canvas(), H, &other, H, W), None);
        }

        #[test]
        fn tiny_inputs_are_rejected() {
            let one = vec![0u8; 4];
            assert_eq!(scroll_offset(&one, 1, &one, 1, 1), None);
        }

        #[test]
        fn append_continues_the_canvas() {
            let mut stitched = Stitched { width: W, height: H, rgba: canvas() };
            let d = 6;
            // A frame fully made of page content: appending its tail extends
            // the canvas with the rows the canvas has never seen.
            let mut frame = Vec::with_capacity(W * H * 4);
            for y in 0..H {
                for x in 0..W {
                    frame.extend_from_slice(&page(x, y + d));
                }
            }
            stitched.append(&frame, H, d);
            assert_eq!(stitched.height, H + d);
            // Canvas row H (first new row) must equal page row H.
            let at = |row: usize, x: usize| &stitched.rgba[(row * W + x) * 4..(row * W + x) * 4 + 4];
            assert_eq!(at(H, 3), &page(3, H));
            assert_eq!(at(H + d - 1, 0), &page(0, H + d - 1));
        }
    }
}

// ------------------------------------------------------------------- the job

/// One running scroll capture. Shared between the capture thread, the IPC
/// commands and the AppState slot.
pub struct Job {
    pub monitor: MonitorInfo,
    /// Display-local physical pixels.
    pub region_px: IRect,
    /// Global logical rect, for the outline window.
    pub region: Rect,
    pub scale: f64,
    pub stop: AtomicBool,
    pub frames: AtomicU32,
    /// Last time the canvas grew (ms epoch, `util::now_ms`).
    pub last_growth: AtomicU64,
    pub image: Mutex<Stitched>,
    /// The previous captured frame, for the settle check: mid-gesture frames
    /// move every tick, so only a repeated frame is worth matching.
    pub last_frame: Mutex<Option<Vec<u8>>>,
    /// The downscaled preview rendered so far — new canvas rows are appended
    /// incrementally instead of rescaling the whole (potentially huge) image
    /// on every stitch.
    pub preview: Mutex<PreviewCache>,
    /// Last `scroll-progress` emission (ms epoch), for the 500 ms throttle.
    pub last_emit: AtomicU64,
    /// `CUTING_SCROLL_DEBUG=<dir>`: settle frames and strips land here for
    /// offline diffing of a misbehaving stitch.
    pub debug_dir: Option<std::path::PathBuf>,
    pub dumps: AtomicU32,
    /// Self test only: pre-generated frames the loop plays back instead of
    /// capturing the screen, so the stitch + finish pipeline runs without
    /// screen-recording permission and fully deterministically.
    pub test_frames: Option<Arc<Vec<(usize, usize, Vec<u8>)>>>,
    pub test_idx: AtomicU32,
    /// The app that was frontmost before the screenshot started; focus goes
    /// back there once the panel closes.
    pub previous_app_pid: Option<i32>,
}

/// The panel preview, kept in sync incrementally.
#[derive(Default)]
pub struct PreviewCache {
    factor: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

pub fn is_active(app: &AppHandle) -> bool {
    lock(&app.state::<AppState>().scroll).is_some()
}

fn take_job(app: &AppHandle) -> Option<Arc<Job>> {
    lock(&app.state::<AppState>().scroll).take()
}

/// Begin a scroll capture from the live screenshot session: take the region's
/// pixels as the first frame, close the session, put up the floating windows
/// and start the capture loop.
///
/// Called from the overlay page, so the selection is display-local CSS.
pub fn start(
    app: &AppHandle,
    label: &str,
    rect: &Rect,
    test_frames: Option<Arc<Vec<(usize, usize, Vec<u8>)>>>,
) -> Result<(), String> {
    if is_active(app) {
        return Err("已有长截图正在进行".into());
    }
    let monitor_id = crate::overlay::monitor_id_from_label(label).ok_or("不是截图覆盖层窗口")?;
    let frame = crate::overlay::frame(app, monitor_id).ok_or("截图会话已结束，请重新截图")?;
    let global = crate::overlay::desktop_rect(app, label, rect).ok_or("截图会话已结束，请重新截图")?;
    if !frame.monitor.rect.contains_rect(&global) {
        return Err("长截图暂不支持跨显示器选区，请在单个屏幕内框选".into());
    }
    let region_px = frame.to_pixels(rect).ok_or("选区太小，请重新框选")?;
    let first = frame.crop_rgba(&region_px);
    let monitor = frame.monitor.clone();
    let scale = frame.scale();
    let previous = crate::overlay::end_session(app);

    let job = Arc::new(Job {
        monitor,
        region_px,
        region: global,
        scale,
        stop: AtomicBool::new(false),
        frames: AtomicU32::new(1),
        last_growth: AtomicU64::new(now_ms() as u64),
        image: Mutex::new(Stitched {
            width: region_px.w as usize,
            height: region_px.h as usize,
            rgba: first,
        }),
        last_frame: Mutex::new(None),
        preview: Mutex::new(PreviewCache::default()),
        last_emit: AtomicU64::new(0),
        debug_dir: std::env::var_os("CUTING_SCROLL_DEBUG").map(std::path::PathBuf::from),
        dumps: AtomicU32::new(0),
        test_frames: test_frames,
        test_idx: AtomicU32::new(0),
        previous_app_pid: previous,
    });
    *lock(&app.state::<AppState>().scroll) = Some(job.clone());

    // Windows go up on the main thread; the first progress event doubles as
    // the "session started" signal for the panel.
    let handle = app.clone();
    let for_windows = job.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = ensure_windows(&handle, &for_windows.region, &for_windows.monitor.rect) {
            clog!("scroll windows: {e}");
        }
        emit_progress(&handle, &for_windows);
    });
    clog!("scroll capture started: region {:?} px on display {}", job.region_px, job.monitor.id);
    spawn_loop(app.clone(), job);
    Ok(())
}

/// Finish and hand the tall image to the save dialog. The panel stays visible
/// ("正在保存…") until the encoding is done — a tall stitch takes seconds, and
/// an instant blank would read as a dead click.
pub async fn save(app: &AppHandle) -> Result<(), String> {
    let job = stop(app).ok_or("没有进行中的长截图")?;
    // The loop is already stopped, so nothing else reads the canvas: take
    // ownership instead of cloning up to a couple hundred megabytes.
    let (width, height, rgba, scale) = {
        let mut image = lock(&job.image);
        (image.width as u32, image.height as u32, std::mem::take(&mut image.rgba), job.scale)
    };
    clog!("scroll save: {}x{} px", width, height);
    let png = tauri::async_runtime::spawn_blocking(move || capture::encode_png(&rgba, width, height, scale))
        .await
        .map_err(|e| e.to_string())??;
    teardown(app);
    crate::commands::spawn_save_dialog(app, png, format!("长截图 {}.png", file_stamp()), job.previous_app_pid);
    Ok(())
}

/// Finish and copy the tall image to the clipboard.
pub fn copy(app: &AppHandle) -> Result<(usize, usize), String> {
    let job = stop(app).ok_or("没有进行中的长截图")?;
    let (width, height, rgba, scale) = {
        let mut image = lock(&job.image);
        (image.width, image.height, std::mem::take(&mut image.rgba), job.scale)
    };
    clog!("scroll copy: {}x{} px", width, height);
    let _ = app.run_on_main_thread(move || {
        let Some(image) = capture::rgba_to_image(&rgba, width as u32, height as u32) else {
            clog!("scroll copy: could not build the image");
            return;
        };
        if let Err(e) = crate::clipboard::write_image(&image, width as f64 / scale, height as f64 / scale) {
            clog!("scroll copy: clipboard failed: {e}");
        }
    });
    teardown(app);
    Ok((width, height))
}

/// Stop without producing anything.
pub fn cancel(app: &AppHandle) {
    if let Some(job) = stop(app) {
        clog!("scroll capture cancelled ({} frame(s))", job.frames.load(Ordering::Relaxed));
        teardown(app);
    }
}

/// Stop the loop and return the job so the caller can still read its canvas.
/// The floating windows stay up: `teardown` removes them once the finisher has
/// something to show (dialog, clipboard). Safe to call from any thread; returns
/// `None` when nothing is running (or another caller already took it).
fn stop(app: &AppHandle) -> Option<Arc<Job>> {
    let job = take_job(app)?;
    job.stop.store(true, Ordering::Relaxed);
    Some(job)
}

/// Take the floating windows down and tell any panel page the session ended
/// (the null event also lifts a stuck busy flag in the panel).
fn teardown(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        for label in [OUTLINE_LABEL, PANEL_LABEL] {
            if let Some(window) = handle.get_webview_window(label) {
                if let Some(ns) = crate::overlay::ns_window(&window) {
                    ns.orderOut(None);
                }
            }
        }
    });
    let _ = app.emit("scroll-progress", serde_json::Value::Null);
}

/// The capture loop: watch the region, stitch whatever is new, report. Runs on
/// its own thread because every ScreenCaptureKit call blocks — and because
/// that thread owns the cached `RegionCapturer` (its Objective-C objects are
/// not `Send`).
fn spawn_loop(app: AppHandle, job: Arc<Job>) {
    std::thread::spawn(move || {
        let mut misses = 0u32;
        let mut ticks = 0u32;
        // The outline and panel windows exist for the whole session, so their
        // ids (and thus the exclusion filter) are stable.
        let exclude = exclude_window_ids(&app);
        // ScreenCaptureKit state is expensive: enumerate shareable content,
        // build the filter and configuration exactly once per session.
        let mut capturer: Option<capture::RegionCapturer> = None;
        let mut full_fallback = false;
        let mut validated = false;
        while !job.stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            if job.stop.load(Ordering::Relaxed) {
                break;
            }
            let mut frame_result: Result<(usize, usize, Vec<u8>), String> = Err("capture not built".into());
            if let Some(tf) = &job.test_frames {
                // Play back the self test's frames instead of the screen.
                let i = (job.test_idx.fetch_add(1, Ordering::Relaxed) as usize).min(tf.len().saturating_sub(1));
                frame_result = Ok(tf[i].clone());
            } else {
                if capturer.is_none() {
                    let built = if full_fallback {
                        capture::RegionCapturer::new_full(&job.monitor, &job.region_px)
                    } else {
                        capture::RegionCapturer::new_region(&job.monitor, &job.region_px, &exclude)
                    };
                    match built {
                        Ok(c) => capturer = Some(c),
                        Err(e) => frame_result = Err(e),
                    }
                }
                if let Some(c) = &capturer {
                    frame_result = c.capture();
                }
            }
            match frame_result {
                Ok((mut w, mut frame_h, mut frame)) => {
                    misses = 0;
                    // Sanity check once per session: the region captured via
                    // `sourceRect` must agree with a plain full-display
                    // capture cropped to the region. If the shortcut's rect
                    // semantics don't match ours, switch to full captures
                    // before any wrong pixels reach the stitcher. (Skipped in
                    // playback mode: there is no real screen to compare.)
                    if !validated && job.test_frames.is_none() {
                        validated = true;
                        clog!("scroll: excluding {} own window(s) from capture", exclude.len());
                        if let Ok((fw, fh, full)) = capture::capture_full_region_rgba(&job.monitor, &job.region_px) {
                            if sampled_diff_fraction(&frame, &full) > 0.05 {
                                clog!("scroll: sourceRect capture disagrees with the full capture; falling back");
                                full_fallback = true;
                                if let Ok(c) = capture::RegionCapturer::new_full(&job.monitor, &job.region_px) {
                                    capturer = Some(c);
                                    frame = full;
                                    w = fw;
                                    frame_h = fh;
                                }
                            }
                        }
                    }
                    // Only stitch once scrolling has settled: mid-gesture
                    // frames are interpolated at fractional offsets that even
                    // the blurred comparison can't align. Two near-identical
                    // consecutive frames are the trigger (a small allowance
                    // keeps blinking cursors and idle animations from
                    // postponing the stitch forever).
                    let (settled, diff_frac) = {
                        let last = lock(&job.last_frame);
                        match last.as_ref() {
                            Some(prev) => {
                                let f = sampled_diff_fraction(prev, &frame);
                                (f <= 0.002, f)
                            }
                            None => (false, 1.0),
                        }
                    };
                    ticks += 1;
                    if ticks % 10 == 0 {
                        clog!("scroll: tick {ticks} @{} ms: frame diff {diff_frac:.4}", (now_ms() as u64) % 100000);
                    }
                    // First ticks: dump regardless of settle, so a
                    // misbehaving session can be inspected frame by frame.
                    if ticks <= 4 {
                        let strip_rows = 24.min(job.image.lock().map(|i| i.height).unwrap_or(0));
                        dump_debug_frame(&job, &frame, &[0u8; 0], w, frame_h, strip_rows);
                    }
                    *lock(&job.last_frame) = Some(frame.clone());
                    if settled {
                        // Match and extract the debug strip under the lock,
                        // then release it: the dump helper writes files and
                        // must never re-lock (it would self-deadlock). Both
                        // the log and the strip are debug-only: a session
                        // parked on a static screen settles every tick, and
                        // this block would otherwise write a log line (file
                        // IO) and memcpy w*24*4 bytes per tick for nothing.
                        let debug = job.debug_dir.is_some();
                        let (found, strip, strip_rows) = {
                            let image = lock(&job.image);
                            let (found, best_mean) =
                                stitch::scroll_offset_scored(&image.rgba, image.height, &frame, frame_h, w);
                            if debug {
                                clog!(
                                    "scroll: settle match: best mean diff {best_mean:.2} (gate 10.00), d = {found:?}, height {}",
                                    image.height
                                );
                            }
                            let strip_rows = 24.min(image.height);
                            let mut strip = Vec::new();
                            if debug {
                                strip = Vec::with_capacity(w * strip_rows * 4);
                                for row in (image.height - strip_rows)..image.height {
                                    let src = row * w * 4;
                                    strip.extend_from_slice(&image.rgba[src..src + w * 4]);
                                }
                            }
                            (found, strip, strip_rows)
                        };
                        dump_debug_frame(&job, &frame, &strip, w, frame_h, strip_rows);
                        let mut image = lock(&job.image);
                        match found {
                            Some(offset) if offset > 0 && offset + frame_h > image.height => {
                                let old_h = image.height;
                                image.append(&frame, frame_h, offset);
                                let height = image.height;
                                let new_rows = height - old_h;
                                drop(image);
                                job.frames.fetch_add(1, Ordering::Relaxed);
                                job.last_growth.store(now_ms() as u64, Ordering::Relaxed);
                                clog!("scroll: +{new_rows} px → {height} px");
                                emit_progress(&app, &job);
                                if height >= MAX_HEIGHT_PX {
                                    clog!("scroll capture hit the {MAX_HEIGHT_PX}px cap, saving");
                                    job.stop.store(true, Ordering::Relaxed);
                                    // `save` encodes on the blocking pool and only
                                    // hops to the main thread for the dialog.
                                    let handle = app.clone();
                                    tauri::async_runtime::spawn(async move {
                                        if let Err(e) = save(&handle).await {
                                            clog!("scroll auto-save failed: {e}");
                                        }
                                    });
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    misses += 1;
                    if misses <= 3 || misses % 10 == 0 {
                        clog!("scroll capture tick failed ({misses}): {e}");
                    }
                    // A stale capturer (invalidated session) keeps failing;
                    // rebuilding gives the next tick a fresh chance.
                    capturer = None;
                    if misses >= MAX_MISSES {
                        clog!("scroll capture gave up after {misses} failed ticks");
                        cancel(&app);
                        break;
                    }
                }
            }
            if (now_ms() as u64) - job.last_growth.load(Ordering::Relaxed) > IDLE_TIMEOUT_MS {
                clog!("scroll capture idle for 10 minutes, cancelling");
                cancel(&app);
                break;
            }
        }
    });
}

/// Fraction of sampled pixels that differ meaningfully between two same-sized
/// buffers. Cheap spot check, not a full comparison.
fn sampled_diff_fraction(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.len() < 4 {
        return 1.0;
    }
    let mut checked = 0usize;
    let mut differing = 0usize;
    let stride = 4 * 397; // prime stride so sampling doesn't alias to rows
    let mut i = 0;
    while i + 4 <= a.len() {
        let diff = a[i..i + 4].iter().zip(&b[i..i + 4]).filter(|(x, y)| x.abs_diff(**y) > 24).count();
        checked += 1;
        if diff > 0 {
            differing += 1;
        }
        i += stride;
    }
    if checked == 0 {
        return 1.0;
    }
    differing as f64 / checked as f64
}

/// With `CUTING_SCROLL_DEBUG=<dir>` set, write the settled frame and the
/// canvas's bottom strip (first few per session), so a misbehaving stitch can
/// be diffed offline: the strip must appear somewhere inside the frame.
///
/// `strip` is extracted by the caller while it holds the canvas lock — this
/// function must not touch `job.image` (the caller already holds it).
fn dump_debug_frame(job: &Job, frame: &[u8], strip: &[u8], w: usize, frame_h: usize, strip_h: usize) {
    let Some(dir) = &job.debug_dir else { return };
    let n = job.dumps.fetch_add(1, Ordering::Relaxed);
    if n >= 6 {
        return;
    }
    let write = |name: String, rgba: &[u8], height: usize| match capture::encode_png(rgba, w as u32, height as u32, 1.0) {
        Ok(png) => {
            if let Err(e) = std::fs::write(dir.join(name), png) {
                clog!("scroll debug dump failed: {e}");
            }
        }
        Err(e) => clog!("scroll debug encode failed: {e}"),
    };
    write(format!("dump{n}-frame.png"), frame, frame_h);
    if !strip.is_empty() {
        write(format!("dump{n}-strip.png"), strip, strip_h);
    }
}

/// Our own floating windows (outline border, control panel) must never appear
/// in the captured region, so the filter excludes them by window id.
fn exclude_window_ids(app: &AppHandle) -> Vec<u32> {
    [OUTLINE_LABEL, PANEL_LABEL]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .filter_map(|window| crate::overlay::ns_window(&window))
        .map(|ns| ns.windowNumber() as u32)
        .collect()
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub width_px: usize,
    pub height_px: usize,
    pub frames: u32,
    /// Small live preview of the stitched image so far (data URL).
    pub preview: String,
}

/// The current state, for a freshly loaded panel page (events fired before the
/// page finished loading would be lost — same pull+push pattern as overlays).
pub fn progress(app: &AppHandle) -> Option<Progress> {
    let job = lock(&app.state::<AppState>().scroll).clone()?;
    Some(progress_of(&job))
}

fn progress_of(job: &Job) -> Progress {
    let image = lock(&job.image);
    let factor = (((image.width as u32) + PREVIEW_WIDTH - 1) / PREVIEW_WIDTH).max(1);
    let (pw, ph, preview) = {
        let mut cache = lock(&job.preview);
        let target_w = image.width as u32 / factor;
        if cache.factor != factor || cache.width != target_w {
            // First render (or a size change, which can't happen mid-session).
            let (w, h, rgba) = capture::downscale_rgba(&image.rgba, image.width as u32, image.height as u32, factor);
            *cache = PreviewCache { factor, width: w, height: h, rgba };
        } else {
            // Incremental: old scaled rows never change, so only the rows
            // below the cache line get downscaled — a fresh 150 px jump costs
            // ~12 scaled rows instead of a full pass over a huge canvas.
            let to = image.height as usize / factor as usize;
            if to > cache.height as usize {
                let (cw, sh, from, ch) = (cache.width as usize, image.height as usize, cache.height as usize, factor as usize);
                append_scaled_rows(&mut cache.rgba, cw, &image.rgba, image.width as usize, sh, from, to, ch);
                cache.height = to as u32;
            }
        }
        (cache.width, cache.height, cache.rgba.clone())
    };
    let (w_px, h_px, frames) = (image.width, image.height, job.frames.load(Ordering::Relaxed));
    drop(image);
    let png = capture::encode_png(&preview, pw, ph, 1.0).unwrap_or_default();
    Progress {
        width_px: w_px,
        height_px: h_px,
        frames,
        preview: format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)),
    }
}

/// Box-downscale canvas rows `[from * factor, to * factor)` and append them to
/// `out` (which already holds `from` scaled rows), one scaled row per
/// `factor` source rows.
fn append_scaled_rows(
    out: &mut Vec<u8>,
    out_w: usize,
    src: &[u8],
    src_w: usize,
    src_h: usize,
    from: usize,
    to: usize,
    factor: usize,
) {
    for y in from..to {
        let y0 = y * factor;
        let y1 = (y0 + factor).min(src_h);
        let rows = (y1 - y0) as u32;
        for x in 0..out_w {
            let x0 = x * factor;
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let base = (sy * src_w + x0) * 4;
                for dx in 0..factor {
                    let i = base + dx * 4;
                    for c in 0..4 {
                        sum[c] += src[i + c] as u32;
                    }
                }
            }
            let n = rows * factor as u32;
            out.extend(sum.map(|v| (v / n) as u8));
        }
    }
}

fn emit_progress(app: &AppHandle, job: &Job) {
    // Throttle: scrolling fast produces many small appends; the panel needs
    // ~2 updates per second, not one per stitch.
    let now = now_ms() as u64;
    let last = job.last_emit.load(Ordering::Relaxed);
    if last != 0 && now - last < PREVIEW_THROTTLE_MS {
        return;
    }
    job.last_emit.store(now, Ordering::Relaxed);
    let _ = app.emit("scroll-progress", progress_of(job));
}

// ------------------------------------------------------------- floating windows

pub(crate) fn ensure_window(app: &AppHandle, label: &str) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(label) {
        return Ok(window);
    }
    WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .incognito(true)
        .title("Cuting 长截图")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|e| format!("创建长截图窗口失败：{e}"))
}

pub(crate) fn configure(level_window: &NSWindow, pass_through: bool) {
    level_window.setLevel(NSScreenSaverWindowLevel);
    level_window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    level_window.setAnimationBehavior(objc2_app_kit::NSWindowAnimationBehavior::None);
    level_window.setHasShadow(false);
    level_window.setIgnoresMouseEvents(pass_through);
}

/// Where the control panel goes: beside the region, falling back to below or
/// above it, always inside the monitor.
fn panel_frame(region: &Rect, monitor: &Rect) -> Rect {
    let (w, h) = PANEL_SIZE;
    let margin = 12.0;
    let mut x = region.right() + margin;
    if x + w > monitor.right() - 4.0 {
        x = region.x - w - margin;
    }
    if x < monitor.x + 4.0 {
        x = region.x.clamp(monitor.x + 4.0, (monitor.right() - w - 4.0).max(monitor.x + 4.0));
    }
    let mut y = region.y.clamp(monitor.y + 4.0, (monitor.bottom() - h - 4.0).max(monitor.y + 4.0));
    if x >= region.right() || region.x >= x + w {
        // Panel is beside the region; keep its top near the region's top.
    } else {
        y = if region.bottom() + margin + h <= monitor.bottom() {
            region.bottom() + margin
        } else {
            (region.y - h - margin).max(monitor.y + 4.0)
        };
    }
    Rect::new(x, y, w, h)
}

/// The self test's scroll driver: a window parked exactly over the capture
/// region whose page scrolls itself, standing in for "the user scrolls some
/// other app". Its content is the pixels under test.
pub(crate) fn ensure_driver_window(app: &AppHandle, rect: &Rect) -> Result<WebviewWindow, String> {
    let window = ensure_window(app, DRIVER_LABEL)?;
    if let Some(ns) = crate::overlay::ns_window(&window) {
        configure(&ns, false);
        ns.setFrame_display(crate::overlay::to_ns_rect(rect), false);
        ns.orderFrontRegardless();
    }
    Ok(window)
}

fn ensure_windows(app: &AppHandle, region: &Rect, monitor_rect: &Rect) -> Result<(), String> {
    MainThreadMarker::new().ok_or("ensure_windows must run on the main thread")?;
    let outline = ensure_window(app, OUTLINE_LABEL)?;
    if let Some(ns) = crate::overlay::ns_window(&outline) {
        configure(&ns, true);
        // The border must never overlap the captured region: the outline
        // window is inflated so its stroke lives entirely outside `region`.
        // Whether or not ScreenCaptureKit honours the window exclusion, the
        // captured pixels stay clean. The ring may extend off-screen at the
        // display's edge — harmless, it just isn't visible there.
        let ring = Rect::new(region.x - 6.0, region.y - 6.0, region.w + 12.0, region.h + 12.0);
        ns.setFrame_display(crate::overlay::to_ns_rect(&ring), false);
        ns.orderFrontRegardless();
    }
    let panel = ensure_window(app, PANEL_LABEL)?;
    if let Some(ns) = crate::overlay::ns_window(&panel) {
        configure(&ns, false);
        ns.setFrame_display(crate::overlay::to_ns_rect(&panel_frame(region, monitor_rect)), false);
        ns.orderFrontRegardless();
        // The panel takes the keys (Enter saves, Esc cancels). Scrolling stays
        // positional on macOS, so this never steals it from the target app.
        ns.makeKeyWindow();
    }
    Ok(())
}
