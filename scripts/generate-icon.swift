import AppKit

guard CommandLine.arguments.count == 2 else {
    fputs("Usage: swift scripts/generate-icon.swift <destination.iconset>\n", stderr)
    exit(2)
}

let destination = CommandLine.arguments[1]
try FileManager.default.createDirectory(atPath: destination, withIntermediateDirectories: true)

let sourceURL = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("assets/AppIcon.png")
let sourceData = try Data(contentsOf: sourceURL)
guard let source = NSBitmapImageRep(data: sourceData)?.cgImage else {
    fputs("Cannot decode assets/AppIcon.png.\n", stderr)
    exit(1)
}
let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!

for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = size * scale
        let context = CGContext(data: nil, width: pixels, height: pixels,
            bitsPerComponent: 8, bytesPerRow: 0, space: colorSpace,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        let factor = CGFloat(pixels) / CGFloat(max(source.width, source.height))
        let width = CGFloat(source.width) * factor
        let height = CGFloat(source.height) * factor
        context.interpolationQuality = .high
        context.draw(source, in: CGRect(x: (CGFloat(pixels) - width) / 2,
            y: (CGFloat(pixels) - height) / 2, width: width, height: height))
        let bitmap = NSBitmapImageRep(cgImage: context.makeImage()!)
        let suffix = scale == 2 ? "@2x" : ""
        let url = URL(fileURLWithPath: destination).appendingPathComponent("icon_\(size)x\(size)\(suffix).png")
        try bitmap.representation(using: .png, properties: [:])!.write(to: url)
    }
}
