import AppKit

let destination = CommandLine.arguments[1]
try FileManager.default.createDirectory(atPath: destination, withIntermediateDirectories: true)

func color(_ rgb: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((rgb >> 16) & 255) / 255,
            green: CGFloat((rgb >> 8) & 255) / 255,
            blue: CGFloat(rgb & 255) / 255, alpha: 1)
}

let tiles: [(CGFloat, CGFloat, CGFloat, CGFloat, UInt32)] = [
    (136, 136, 418, 452, 0x49B9A6),
    (136, 604, 201, 284, 0x49B9A6),
    (353, 604, 201, 284, 0x49B9A6),
    (570, 518, 318, 370, 0x6897DA),
    (570, 330, 151, 172, 0x6897DA),
    (737, 330, 151, 172, 0x6897DA),
    (570, 136, 318, 178, 0xD5A465),
]

for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = size * scale
        let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels,
            pixelsHigh: pixels, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
            bytesPerRow: 0, bitsPerPixel: 0)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
        let transform = AffineTransform(scale: CGFloat(pixels) / 1024)
        (transform as NSAffineTransform).concat()
        color(0x172029).setFill()
        NSBezierPath(roundedRect: NSRect(x: 48, y: 48, width: 928, height: 928),
            xRadius: 204, yRadius: 204).fill()
        for (x, y, width, height, rgb) in tiles {
            color(rgb).setFill()
            NSBezierPath(roundedRect: NSRect(x: x, y: y, width: width, height: height),
                xRadius: 10, yRadius: 10).fill()
        }
        NSGraphicsContext.restoreGraphicsState()
        let suffix = scale == 2 ? "@2x" : ""
        let url = URL(fileURLWithPath: destination).appendingPathComponent("icon_\(size)x\(size)\(suffix).png")
        try bitmap.representation(using: .png, properties: [:])!.write(to: url)
    }
}
