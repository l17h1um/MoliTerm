// Renders assets/icons/MoliTerm.iconset PNGs. Run: swift scripts/make-icon.swift && iconutil -c icns assets/icons/MoliTerm.iconset
import AppKit

let out = "assets/icons/MoliTerm.iconset"
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)
for (px, name) in [(16, "16x16"), (32, "16x16@2x"), (32, "32x32"), (64, "32x32@2x"), (128, "128x128"),
                   (256, "128x128@2x"), (256, "256x256"), (512, "256x256@2x"), (512, "512x512"), (1024, "512x512@2x")] {
    let s = CGFloat(px)
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let inset = s * 0.09
    let box = NSRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    NSColor(red: 0.043, green: 0.055, blue: 0.078, alpha: 1).setFill()
    NSBezierPath(roundedRect: box, xRadius: s * 0.18, yRadius: s * 0.18).fill()
    let accent = NSColor(red: 0.902, green: 0.706, blue: 0.314, alpha: 1)
    accent.setStroke()
    let p = NSBezierPath()
    p.lineWidth = s * 0.07
    p.lineCapStyle = .round
    p.lineJoinStyle = .round
    p.move(to: NSPoint(x: s * 0.27, y: s * 0.66))
    p.line(to: NSPoint(x: s * 0.45, y: s * 0.5))
    p.line(to: NSPoint(x: s * 0.27, y: s * 0.34))
    p.move(to: NSPoint(x: s * 0.52, y: s * 0.32))
    p.line(to: NSPoint(x: s * 0.73, y: s * 0.32))
    p.stroke()
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "\(out)/icon_\(name).png"))
}
