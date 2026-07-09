import SwiftUI
import UIKit

extension Color {
    /// Builds a color that resolves differently in light and dark mode.
    init(light: Color, dark: Color) {
        self = Color(UIColor { traits in
            UIColor(traits.userInterfaceStyle == .dark ? dark : light)
        })
    }

    /// Filestash brand accent — a blue that stays legible in both appearances.
    static let fsAccent = Color(
        light: Color(red: 0.10, green: 0.46, blue: 0.78),
        dark: Color(red: 0.53, green: 0.80, blue: 0.95)
    )

    /// Status circle colors, shared with the Android app.
    static let fsConnected = Color(red: 0x63 / 255, green: 0xD9 / 255, blue: 0xB1 / 255)
    static let fsOffline = Color(red: 0xF2 / 255, green: 0x6D / 255, blue: 0x6D / 255)
    /// Dark glyph drawn on top of the connected/offline fill.
    static let fsGlyph = Color(red: 0x24 / 255, green: 0x27 / 255, blue: 0x2A / 255)
}
