// Renders an SVG to a transparent PNG at an exact pixel size using AppKit's SVG support.
// Usage: swift rasterize.swift <in.svg> <out.png> <pixels>
import AppKit

let args = CommandLine.arguments
guard args.count == 4, let pixels = Int(args[3]), pixels > 0 else {
    FileHandle.standardError.write("usage: rasterize <in.svg> <out.png> <pixels>\n".data(using: .utf8)!)
    exit(2)
}
guard let image = NSImage(contentsOf: URL(fileURLWithPath: args[1])) else {
    FileHandle.standardError.write("cannot load \(args[1])\n".data(using: .utf8)!)
    exit(1)
}
guard let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
) else { exit(1) }
rep.size = NSSize(width: pixels, height: pixels)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high
image.draw(in: NSRect(x: 0, y: 0, width: pixels, height: pixels), from: .zero, operation: .copy, fraction: 1)
NSGraphicsContext.restoreGraphicsState()

guard let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
do {
    try png.write(to: URL(fileURLWithPath: args[2]))
} catch {
    FileHandle.standardError.write("cannot write \(args[2]): \(error)\n".data(using: .utf8)!)
    exit(1)
}
