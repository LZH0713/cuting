//! Text recognition with Apple's Vision framework: on-device, free, and good
//! at mixed Chinese/English screenshots.

use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSDictionary, NSString};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizedTextObservation, VNRecognizeTextRequest,
    VNRequest, VNRequestTextRecognitionLevel,
};

/// One recognized line. The box is normalized to the image (0..1) with the
/// origin at the top-left, matching what the frontend draws.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Recognition languages to try first, in priority order.
pub const OCR_LANGUAGES: &[(&str, &str)] = &[
    ("zh-Hans", "简体中文"),
    ("zh-Hant", "繁體中文"),
    ("en-US", "英语"),
    ("ja-JP", "日语"),
    ("ko-KR", "韩语"),
];

/// Recognition quality level. Accurate is the default; fast trades accuracy
/// for a small model that never touches the ANE program the accurate level
/// needs. Measured on this machine (macOS 27.2.1, ANECompilerService wedged
/// after the update): accurate stalls ~30 s on a fresh process, fast runs in
/// ~30-50 ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Accurate,
    Fast,
}

// ------------------------------------------------------------------- health
//
// A Vision pipeline that hung once tends to hang again for a while (the ANE
// compiler service gets wedged after an OS update, then recovers). Requests
// can't be cancelled — `performRequests` just never returns and its worker
// thread is stranded — so instead of letting retries pile up stranded
// threads, remember the hang and let callers route around it.

/// How long after a hang accurate recognition is considered unsafe.
const COOLDOWN_MS: u64 = 60_000;

static LAST_HANG_MS: AtomicU64 = AtomicU64::new(0);

/// Record that an accurate request hung past the caller's give-up deadline.
pub fn mark_hang() {
    LAST_HANG_MS.store(crate::util::now_ms() as u64, Ordering::Relaxed);
}

/// True while a recent hang makes accurate recognition likely to stall again.
pub fn cooling_down() -> bool {
    let last = LAST_HANG_MS.load(Ordering::Relaxed);
    last != 0 && crate::util::now_ms() as u64 - last < COOLDOWN_MS
}

/// Recognize text in `image` at the accurate level. CPU-bound and synchronous;
/// run it on a blocking thread. Within the cooldown after a hang this fails
/// fast with a friendly error instead of queueing behind a wedged pipeline.
pub fn recognize(image: &CGImage, languages: &[String]) -> Result<Vec<OcrLine>, String> {
    if cooling_down() {
        return Err(
            "系统识别服务刚刚卡顿过，正在等待其恢复，请稍后再试（工具会自动改用快速识别）".into(),
        );
    }
    recognize_level(image, languages, Level::Accurate)
}

/// The fast level: near-instant and immune to the ANE stall. This is the
/// fallback when accurate hangs, so it deliberately skips the cooldown gate.
pub fn recognize_fast(image: &CGImage, languages: &[String]) -> Result<Vec<OcrLine>, String> {
    recognize_level(image, languages, Level::Fast)
}

/// Recognize text in `image` at `level`. CPU-bound and synchronous; run it on
/// a blocking thread. An empty `languages` list lets Vision detect the script
/// itself.
pub fn recognize_level(
    image: &CGImage,
    languages: &[String],
    level: Level,
) -> Result<Vec<OcrLine>, String> {
    // Vision autoreleases heavily, and blocking-pool threads have no autorelease
    // pool of their own: without one every request would leak until the thread dies.
    objc2::rc::autoreleasepool(|_| recognize_inner(image, languages, level))
}

fn recognize_inner(
    image: &CGImage,
    languages: &[String],
    level: Level,
) -> Result<Vec<OcrLine>, String> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(match level {
        Level::Accurate => VNRequestTextRecognitionLevel::Accurate,
        Level::Fast => VNRequestTextRecognitionLevel::Fast,
    });
    if languages.is_empty() {
        request.setAutomaticallyDetectsLanguage(true);
    } else {
        let langs: Vec<Retained<NSString>> = languages.iter().map(|l| NSString::from_str(l)).collect();
        request.setRecognitionLanguages(&NSArray::from_retained_slice(&langs));
    }
    request.setUsesLanguageCorrection(true);

    let options: Retained<NSDictionary<VNImageOption, objc2::runtime::AnyObject>> = NSDictionary::new();
    // SAFETY: `alloc` is paired with `init`; `image` outlives the call.
    let handler = unsafe { VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), image, &options) };
    let as_request: &VNRequest = &request;
    handler
        .performRequests_error(&NSArray::from_slice(&[as_request]))
        .map_err(|e| format!("文字识别失败：{}", e.localizedDescription().to_string()))?;

    let observations = request.results().map(|r| r.to_vec()).unwrap_or_default();
    let mut lines: Vec<OcrLine> = observations
        .into_iter()
        .filter_map(|obs| top_candidate(&obs))
        .collect();
    lines.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal).then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal)));
    Ok(lines)
}

/// Warm the accurate pipeline once, in the background: the first request in a
/// process pays a model load that can stall for tens of seconds on a machine
/// with a wedged ANE compiler service. Paying it invisibly at startup keeps
/// the user's first screenshot snappy. Never panics, never errors outward.
pub fn prewarm(languages: Vec<String>) {
    let spawned = std::thread::Builder::new().name("ocr-prewarm".into()).spawn(move || {
        // Any small image works: the point is loading the model, not the text.
        let (w, h) = (128usize, 32usize);
        let mut rgba = vec![255u8; w * h * 4];
        for row in 8..16 {
            for x in (0..w).step_by(2) {
                let at = (row * w + x) * 4;
                rgba[at..at + 3].fill(0);
            }
        }
        let Some(image) = crate::capture::rgba_to_image(&rgba, w as u32, h as u32) else {
            crate::clog!("ocr prewarm: could not build the warm-up image");
            return;
        };
        objc2::rc::autoreleasepool(|_| {
            let started = crate::util::now_ms();
            match recognize_inner(&image, &languages, Level::Accurate) {
                Ok(_) => crate::clog!("ocr prewarm: pipeline ready in {} ms", crate::util::now_ms() - started),
                Err(e) => crate::clog!("ocr prewarm failed: {e}"),
            }
        });
    });
    if spawned.is_err() {
        crate::clog!("ocr prewarm: could not spawn the warm-up thread");
    }
}

fn top_candidate(obs: &VNRecognizedTextObservation) -> Option<OcrLine> {
    // SAFETY: plain getters on a Vision observation.
    let (text, box_) = unsafe {
        let text = obs.topCandidates(1).firstObject()?;
        (text.string().to_string(), obs.boundingBox())
    };
    Some(OcrLine {
        text,
        x: box_.origin.x,
        y: 1.0 - (box_.origin.y + box_.size.height),
        w: box_.size.width,
        h: box_.size.height,
    })
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF
    )
}

/// Two lines sit in the same visual row when their centers are closer than 60%
/// of the taller line's height.
fn same_row(a: &OcrLine, b: &OcrLine) -> bool {
    let (ca, cb) = (a.y + a.h / 2.0, b.y + b.h / 2.0);
    (ca - cb).abs() <= a.h.max(b.h) * 0.6
}

/// Join lines into text: a line break between rows, and between neighbors in
/// one row a space unless both sides are CJK (Chinese doesn't use spaces).
pub fn lines_to_text(lines: &[OcrLine]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            let prev = &lines[i - 1];
            if !same_row(prev, line) {
                out.push('\n');
            } else {
                let no_space = prev.text.chars().last().is_some_and(is_cjk) && line.text.chars().next().is_some_and(is_cjk);
                if !no_space {
                    out.push(' ');
                }
            }
        }
        out.push_str(&line.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x: f64, y: f64) -> OcrLine {
        OcrLine { text: text.to_string(), x, y, w: 0.1, h: 0.05 }
    }

    #[test]
    fn rows_and_cjk_spacing() {
        let lines = vec![
            line("Hello", 0.0, 0.0),
            line("world", 0.2, 0.0),
            line("你好", 0.0, 0.1),
            line("世界", 0.2, 0.1),
            line("混合mix", 0.4, 0.1),
        ];
        assert_eq!(lines_to_text(&lines), "Hello world\n你好世界混合mix");
    }

    #[test]
    fn slightly_offset_lines_stay_in_one_row() {
        let a = OcrLine { text: "a".into(), x: 0.0, y: 0.100, w: 0.1, h: 0.040 };
        let b = OcrLine { text: "b".into(), x: 0.2, y: 0.105, w: 0.1, h: 0.040 };
        assert!(same_row(&a, &b));
        let c = OcrLine { text: "c".into(), x: 0.2, y: 0.150, w: 0.1, h: 0.040 };
        assert!(!same_row(&a, &c));
    }

    /// End-to-end through the real Vision framework. Needs the fixture from
    /// `scripts/make-ocr-fixture.swift`; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs scripts/make-ocr-fixture.swift to run first"]
    fn vision_recognizes_fixture() {
        let bytes = std::fs::read("target/ocr-fixture.png").expect("fixture; run scripts/make-ocr-fixture.swift");
        let (_, _, rgba) = crate::capture::decode_png(&bytes).unwrap();
        let image = crate::capture::rgba_to_image(&rgba, 900, 220).unwrap();
        let lines = recognize(&image, &[]).expect("recognition");
        let text = lines_to_text(&lines);
        assert!(text.contains("Hello"), "english missing in: {text:?}");
        assert!(text.contains("你好"), "chinese missing in: {text:?}");
        assert!(text.contains("12345"), "digits missing in: {text:?}");
    }
}
