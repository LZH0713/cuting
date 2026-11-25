//! Renders the OCR test fixture with CoreText. Run from the repo root:
//!
//! ```sh
//! swift src-tauri/scripts/make-ocr-fixture.swift
//! ```
//!
//! Writes `target/ocr-fixture.png`: black text on white, which the Rust
//! `vision_recognizes_fixture` test (run with `cargo test -- --ignored`)
//! feeds through the real Vision pipeline.

import Foundation
import CoreGraphics
import CoreText
import ImageIO
import UniformTypeIdentifiers

let width = 900
let height = 220
let space = CGColorSpace(name: CGColorSpace.sRGB)!
let context = CGContext(
    data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
context.fill(CGRect(x: 0, y: 0, width: width, height: height))

let attributed = NSAttributedString(
    string: "Hello Cuting 你好世界 12345",
    attributes: [
        NSAttributedString.Key(kCTFontAttributeName as String): CTFontCreateWithName("PingFang SC" as CFString, 56, nil),
        NSAttributedString.Key(kCTForegroundColorAttributeName as String): CGColor(red: 0, green: 0, blue: 0, alpha: 1),
    ]
)
let line = CTLineCreateWithAttributedString(attributed)
context.textPosition = CGPoint(x: 40, y: 80)
CTLineDraw(line, context)

let out = URL(fileURLWithPath: "target/ocr-fixture.png") as CFURL
let destination = CGImageDestinationCreateWithURL(out, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(destination, context.makeImage()!, nil)
guard CGImageDestinationFinalize(destination) else { fatalError("could not write fixture") }
print("wrote target/ocr-fixture.png")
