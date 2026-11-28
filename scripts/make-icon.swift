// Draws the app icon: the word "diayti" in glowing brush script, with a brush stroke under
// it, on a night-blue tile with a thin white frame.
//
//   swift scripts/make-icon.swift          (from the project folder, on macOS)
//
// It writes the two images the builds use:
//
//   assets/icon-macos.png    1024 px, laid out like a macOS app icon (margin and shadow);
//                            scripts/bundle-macos.sh turns it into the bundle's .icns
//   assets/icon-window.png   256 px, the tile edge to edge; compiled into the program as
//                            the window and taskbar icon on Windows
//
// To change the look, edit the constants below and run it again.

import AppKit
import CoreText
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

let word = "diayti"
/// A brush script that ships with macOS.
let fontName = "SignPainter-HouseScriptSemibold"
/// The tile's colours, from top to bottom.
let tileColors: [UInt32] = [0x30316A, 0x17183A]
/// Corner radius of the tile as a share of its width, with the smooth ("continuous")
/// rounding of Apple's icons. Recent macOS trims every app icon to about this outline; a
/// tile with tighter corners would lose the frame there.
let cornerShare: CGFloat = 0.265
/// Width of the white frame just inside the tile's edge, as a share of the tile's width.
let frameShare: CGFloat = 0.02
/// The lettering's colours, from its bottom-left to its top-right.
let inkColors: [UInt32] = [0x8FA3FF, 0xC59BFF, 0xFF9CCB]
/// The colour the lettering glows in.
let glow: UInt32 = 0x7C6CFF
/// Counter-clockwise tilt of the lettering, in degrees.
let tilt: CGFloat = 7
/// The most room the lettering may take, as shares of the tile's width and height.
let widthShare: CGFloat = 0.78
let heightShare: CGFloat = 0.48

let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

func color(_ hex: UInt32, alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255,
        green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: alpha)
}

/// A gradient through `colors`, evenly spaced.
func gradient(_ colors: [UInt32]) -> CGGradient {
    let stops = colors.indices.map { CGFloat($0) / CGFloat(max(1, colors.count - 1)) }
    return CGGradient(colorsSpace: srgb, colors: colors.map { color($0) } as CFArray, locations: stops)!
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

/// The outline of the word plus the brush stroke under it, in the font's own coordinates.
func lettering() -> CGPath {
    let font = CTFontCreateWithName(fontName as CFString, 200, nil)
    // An unknown name silently yields a default font, and with it a different icon.
    guard CTFontCopyPostScriptName(font) as String == fontName else {
        fail("The font \(fontName) is not installed on this Mac.")
    }
    let attributes = [NSAttributedString.Key(kCTFontAttributeName as String): font]
    let line = CTLineCreateWithAttributedString(NSAttributedString(string: word, attributes: attributes))

    let path = CGMutablePath()
    for run in CTLineGetGlyphRuns(line) as! [CTRun] {
        let runFont = (CTRunGetAttributes(run) as NSDictionary)[kCTFontAttributeName as String] as! CTFont
        let count = CTRunGetGlyphCount(run)
        var glyphs = [CGGlyph](repeating: 0, count: count)
        var positions = [CGPoint](repeating: .zero, count: count)
        CTRunGetGlyphs(run, CFRange(location: 0, length: 0), &glyphs)
        CTRunGetPositions(run, CFRange(location: 0, length: 0), &positions)
        for index in 0..<count {
            if let glyph = CTFontCreatePathForGlyph(runFont, glyphs[index], nil) {
                let position = CGAffineTransform(translationX: positions[index].x, y: positions[index].y)
                path.addPath(glyph, transform: position)
            }
        }
    }

    // The stroke: two curves between the same end points, so it swells in the middle and
    // comes to a point at both ends, rising a little to the right.
    let box = path.boundingBoxOfPath
    let left = CGPoint(x: box.minX + box.width * 0.04, y: box.minY - box.height * 0.20)
    let right = CGPoint(x: box.maxX + box.width * 0.03, y: box.minY - box.height * 0.02)
    let middle = CGPoint(x: (left.x + right.x) / 2, y: (left.y + right.y) / 2)
    path.move(to: left)
    path.addQuadCurve(to: right, control: CGPoint(x: middle.x, y: middle.y - box.height * 0.10))
    path.addQuadCurve(to: left, control: CGPoint(x: middle.x - box.width * 0.02, y: middle.y - box.height * 0.26))
    path.closeSubpath()
    return path
}

/// Draw the icon `size` pixels square. `margin` is the empty border around the tile as a
/// share of the canvas.
func render(size: Int, margin: CGFloat, shadow: Bool, to file: String) {
    let side = CGFloat(size)
    let context = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: srgb,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

    let tile = CGRect(x: 0, y: 0, width: side, height: side).insetBy(dx: side * margin, dy: side * margin)
    let shape = RoundedRectangle(cornerRadius: tile.width * cornerShare, style: .continuous).path(in: tile).cgPath

    if shadow {
        context.saveGState()
        context.setShadow(
            offset: CGSize(width: 0, height: -side * 0.010), blur: side * 0.022,
            color: color(0x1B1F3A, alpha: 0.28))
        context.addPath(shape)
        context.setFillColor(color(tileColors[tileColors.count / 2]))
        context.fillPath()
        context.restoreGState()
    }

    context.saveGState()
    context.addPath(shape)
    context.clip()
    context.drawLinearGradient(
        gradient(tileColors), start: CGPoint(x: tile.midX, y: tile.maxY), end: CGPoint(x: tile.midX, y: tile.minY),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    // The frame: a stroke along the edge, of which the clip keeps the inner half.
    context.addPath(shape)
    context.setStrokeColor(color(0xFFFFFF))
    context.setLineWidth(tile.width * frameShare * 2)
    context.strokePath()
    context.restoreGState()

    // Fit the lettering into its share of the tile, tilt it and centre it.
    let art = lettering()
    let box = art.boundingBoxOfPath
    let scale = min(tile.width * widthShare / box.width, tile.height * heightShare / box.height)
    var placement = CGAffineTransform(translationX: tile.midX, y: tile.midY)
        .rotated(by: tilt * .pi / 180)
        .scaledBy(x: scale, y: scale)
        .translatedBy(x: -box.midX, y: -box.midY)
    let placed = art.copy(using: &placement)!
    let placedBox = placed.boundingBoxOfPath

    // First the glow, as the blurred shadow of the lettering in the glow colour…
    context.saveGState()
    context.setShadow(offset: .zero, blur: side * 0.035, color: color(glow, alpha: 0.75))
    context.addPath(placed)
    context.setFillColor(color(glow))
    context.fillPath()
    context.restoreGState()
    // …then the lettering itself.
    context.saveGState()
    context.addPath(placed)
    context.clip()
    context.drawLinearGradient(
        gradient(inkColors), start: CGPoint(x: placedBox.minX, y: placedBox.minY),
        end: CGPoint(x: placedBox.maxX, y: placedBox.maxY),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    context.restoreGState()

    let url = URL(fileURLWithPath: file)
    let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(destination, context.makeImage()!, nil)
    guard CGImageDestinationFinalize(destination) else {
        fail("could not write \(file)")
    }
    print("wrote \(file)")
}

try FileManager.default.createDirectory(atPath: "assets", withIntermediateDirectories: true)
// Apple's layout for app icons: an 824 px tile on a 1024 px canvas.
render(size: 1024, margin: 100.0 / 1024.0, shadow: true, to: "assets/icon-macos.png")
render(size: 256, margin: 0.02, shadow: false, to: "assets/icon-window.png")
