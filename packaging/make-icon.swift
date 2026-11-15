import AppKit
let destination = CommandLine.arguments[1]
let sizes = [16, 32, 128, 256, 512]
try FileManager.default.createDirectory(atPath: destination, withIntermediateDirectories: true)
for size in sizes {
    for scale in [1, 2] {
        let pixels = size * scale
        let image = NSImage(size: NSSize(width: pixels, height: pixels))
        image.lockFocus()
        let factor = CGFloat(pixels) / 1024
        let transform = NSAffineTransform(); transform.scale(by: factor); transform.concat()
        let tile = NSBezierPath(roundedRect: NSRect(x: 44, y: 44, width: 936, height: 936), xRadius: 214, yRadius: 214)
        NSGradient(starting: NSColor(srgbRed: 0.16, green: 0.48, blue: 0.42, alpha: 1), ending: NSColor(srgbRed: 0.06, green: 0.24, blue: 0.23, alpha: 1))!.draw(in: tile, angle: -60)
        NSColor(srgbRed: 0.76, green: 0.93, blue: 0.84, alpha: 1).setFill()
        let left = NSBezierPath(roundedRect: NSRect(x: 234, y: 312, width: 104, height: 400), xRadius: 52, yRadius: 52); left.fill()
        let right = NSBezierPath(roundedRect: NSRect(x: 686, y: 312, width: 104, height: 400), xRadius: 52, yRadius: 52); right.fill()
        let bridge = NSBezierPath()
        bridge.move(to: NSPoint(x: 286, y: 505))
        bridge.curve(to: NSPoint(x: 738, y: 505), controlPoint1: NSPoint(x: 400, y: 680), controlPoint2: NSPoint(x: 624, y: 680))
        bridge.lineWidth = 86; bridge.lineCapStyle = .round
        NSColor.white.setStroke(); bridge.stroke()
        NSColor(srgbRed: 0.94, green: 0.77, blue: 0.42, alpha: 1).setFill()
        NSBezierPath(ovalIn: NSRect(x: 456, y: 294, width: 112, height: 112)).fill()
        image.unlockFocus()
        let bitmap = NSBitmapImageRep(data: image.tiffRepresentation!)!
        let data = bitmap.representation(using: .png, properties: [:])!
        let filename = "icon_\(size)x\(size)" + (scale == 2 ? "@2x" : "") + ".png"
        try data.write(to: URL(fileURLWithPath: destination).appendingPathComponent(filename))
    }
}
