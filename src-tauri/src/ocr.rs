//! Text recognition with Apple's Vision framework: on-device, free, and good
//! at mixed Chinese/English screenshots.

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

/// Recognize text in `image`. CPU-bound and synchronous; run it on a blocking
/// thread. An empty `languages` list lets Vision detect the script itself.
pub fn recognize(image: &CGImage, languages: &[String]) -> Result<Vec<OcrLine>, String> {
    // Vision autoreleases heavily, and blocking-pool threads have no autorelease
    // pool of their own: without one every request would leak until the thread dies.
    objc2::rc::autoreleasepool(|_| recognize_inner(image, languages))
}

fn recognize_inner(image: &CGImage, languages: &[String]) -> Result<Vec<OcrLine>, String> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
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
