//! Renders the icon sources with CoreGraphics. Run from the repo root:
//!
//! ```sh
//! swift src-tauri/scripts/make-icons.swift
//! ```
//!
//! It writes `icons/tray.png` (menu bar glyph, 44 px so it stays sharp on
//! Retina) and `icons/app-icon.png` (1024 px source for `tauri icon`).

import Foundation
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

func makeContext(_ size: Int) -> CGContext {
    let space = CGColorSpace(name: CGColorSpace.sRGB)!
    return CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: size * 4,
        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
}

func write(_ context: CGContext, to path: String) {
    let url = URL(fileURLWithPath: path) as CFURL
    let destination = CGImageDestinationCreateWithURL(url, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(destination, context.makeImage()!, nil)
    guard CGImageDestinationFinalize(destination) else { fatalError("could not write \(path)") }
    print("wrote \(path)")
}

/// A selection frame with a gap in the middle of each side: four corner brackets.
func drawBrackets(_ context: CGContext, from m: CGFloat, to s: CGFloat, length L: CGFloat, width: CGFloat) {
    context.setLineWidth(width)
    context.setLineCap(.round)
    context.beginPath()
    // Each bracket runs in from one side of a corner and out along the other.
    let corners: [((CGFloat, CGFloat), (CGFloat, CGFloat), (CGFloat, CGFloat))] = [
        ((m, s - L), (m, s), (m + L, s)),  // top-left
        ((s - L, s), (s, s), (s, s - L)),  // top-right
        ((s, m + L), (s, m), (s - L, m)),  // bottom-right
        ((m + L, m), (m, m), (m, m + L)),  // bottom-left
    ]
    for (a, b, c) in corners {
        context.move(to: CGPoint(x: a.0, y: a.1))
        context.addLine(to: CGPoint(x: b.0, y: b.1))
        context.addLine(to: CGPoint(x: c.0, y: c.1))
    }
    context.strokePath()
}

let iconsDir = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "src-tauri/icons").path

// Menu bar glyph: black + alpha, used as a template image.
let tray = makeContext(44)
tray.clear(CGRect(x: 0, y: 0, width: 44, height: 44))
tray.setStrokeColor(CGColor(red: 0, green: 0, blue: 0, alpha: 1))
drawBrackets(tray, from: 5, to: 39, length: 12, width: 4)
write(tray, to: "\(iconsDir)/tray.png")

// App icon: rounded tile with a gradient and the same brackets in white.
let app = makeContext(1024)
app.clear(CGRect(x: 0, y: 0, width: 1024, height: 1024))
let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
app.addPath(CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil))
app.clip()
let colors = [
    CGColor(red: 0.24, green: 0.51, blue: 1.00, alpha: 1),
    CGColor(red: 0.42, green: 0.36, blue: 0.98, alpha: 1),
] as CFArray
let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!, colors: colors, locations: [0, 1])!
app.drawLinearGradient(
    gradient, start: CGPoint(x: 100, y: 924), end: CGPoint(x: 924, y: 100), options: []
)
app.setStrokeColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
drawBrackets(app, from: 320, to: 704, length: 150, width: 64)
write(app, to: "\(iconsDir)/app-icon.png")
