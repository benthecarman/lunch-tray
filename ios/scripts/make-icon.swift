// Renders the launcher icon from the desktop app's brand.rs geometry:
// the mint tray mark on a slate tile, full bleed for iOS's mask.
// Usage: swift scripts/make-icon.swift LunchTray/Assets.xcassets/AppIcon.appiconset/AppIcon.png
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024.0
let out = CommandLine.arguments[1]
// No alpha: App Store icons must be opaque.
let cg = CGContext(
    data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!

func rgb(_ hex: UInt32) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

cg.setFillColor(rgb(0x1d2320))
cg.fill(CGRect(x: 0, y: 0, width: size, height: size))

// The mark at 60% of the tile, as in app_icon_svg: a rim of a tenth of its
// width, squashed vertically, and a divider left of center.
let s = size * 0.6
cg.translateBy(x: size / 2, y: size / 2)
cg.scaleBy(x: s, y: s * 0.7407)
cg.setStrokeColor(rgb(0xe9ede8))
cg.setLineWidth(0.1)
cg.addPath(
    CGPath(
        roundedRect: CGRect(x: -0.39, y: -0.39, width: 0.78, height: 0.78), cornerWidth: 0.11,
        cornerHeight: 0.11, transform: nil))
cg.strokePath()
cg.setLineWidth(0.09)
cg.move(to: CGPoint(x: -0.1, y: -0.34))
cg.addLine(to: CGPoint(x: -0.1, y: 0.34))
cg.strokePath()

let dest = CGImageDestinationCreateWithURL(
    URL(fileURLWithPath: out) as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, cg.makeImage()!, nil)
precondition(CGImageDestinationFinalize(dest))
