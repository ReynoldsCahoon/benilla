// The app icon from an .ico: its largest image, scaled to 1024 px over black (iOS icons have no
// alpha; iOS would fill the corners black itself).
//
//   swift tools/icon.swift <in.ico> <out.png>
import CoreGraphics
import Foundation
import ImageIO

let args = CommandLine.arguments
guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, nil) else {
    fatalError("cannot read \(args[1])")
}
var best: CGImage?
for i in 0..<CGImageSourceGetCount(source) {
    if let image = CGImageSourceCreateImageAtIndex(source, i, nil),
        image.width > (best?.width ?? 0)
    {
        best = image
    }
}
guard let image = best else { fatalError("no image in \(args[1])") }
let size = 1024
let ctx = CGContext(
    data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
ctx.setFillColor(CGColor(gray: 0, alpha: 1))
ctx.fill(CGRect(x: 0, y: 0, width: size, height: size))
ctx.interpolationQuality = .high
ctx.draw(image, in: CGRect(x: 0, y: 0, width: size, height: size))
let dest = CGImageDestinationCreateWithURL(
    URL(fileURLWithPath: args[2]) as CFURL, "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
guard CGImageDestinationFinalize(dest) else { fatalError("cannot write \(args[2])") }
