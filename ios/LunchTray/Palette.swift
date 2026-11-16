// The tray palette from the desktop app's `style.rs`: a mint-grey melamine
// surface, a recessed compartment per section, and mustard for the few
// things that need attention. Light and dark share the design.

import SwiftUI
import UIKit

enum Palette {
    static let bg = dynamic(0xe9ede8, 0x1d2320)
    static let well = dynamic(0xdfe5e0, 0x161b18)
    static let field = dynamic(0xf5f7f5, 0x111512)
    static let text = dynamic(0x18211c, 0xe6ece7)
    static let textWeak = dynamic(0x5e6a63, 0x93a098)
    /// Mustard: attention, pins, selection. Never body text.
    static let accent = dynamic(0xc28f12, 0xe0b23a)
    static let open = dynamic(0x2f8f4e, 0x4cb56a)
    static let merged = dynamic(0x7b4fb0, 0xa98bd6)
    static let closed = dynamic(0xb8412f, 0xe0654f)
    static let warning = dynamic(0xa47500, 0xe0b23a)
    static let danger = dynamic(0xb8412f, 0xe0654f)

    private static func dynamic(_ light: UInt32, _ dark: UInt32) -> Color {
        Color(
            uiColor: UIColor { traits in
                rgb(traits.userInterfaceStyle == .dark ? dark : light)
            })
    }

    private static func rgb(_ hex: UInt32) -> UIColor {
        UIColor(
            red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: 1)
    }
}

/// The Lunch Tray mark: a wide rounded tray with two compartments, as in
/// the desktop app's `brand.rs`. Stroke it with a tenth of its width.
struct TrayMark: Shape {
    func path(in rect: CGRect) -> Path {
        let w = rect.width
        // The rim's centerline, squashed vertically like the desktop mark.
        let frame = CGRect(
            x: rect.midX - w * 0.39, y: rect.midY - w * 0.39 * 0.7407,
            width: w * 0.78, height: w * 0.78 * 0.7407)
        var p = Path(roundedRect: frame, cornerRadius: w * 0.11)
        let x = rect.midX - w * 0.1
        p.move(to: CGPoint(x: x, y: frame.minY + w * 0.05))
        p.addLine(to: CGPoint(x: x, y: frame.maxY - w * 0.05))
        return p
    }
}
