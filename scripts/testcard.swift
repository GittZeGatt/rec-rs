// A static text test card for capture-quality measurements.
//
//   swift scripts/testcard.swift OUT.png [SECONDS [screen [front]]]
//
// Draws a deterministic 5120x2880 card (UI text at several sizes, code,
// colored text, 1 px lines, a gradient) into OUT.png and, if SECONDS is
// given, shows it 1:1 in a borderless window on a 2x display of that size
// (with `screen`: on the first 2x display, the card sized to it; with
// `front`: in front of other windows, for display-capture checks),
// ordered behind other windows (window capture reads it anyway). Prints
// `window <id>` once it is on screen. scripts/quality.sh uses it.

import AppKit

let args = CommandLine.arguments
let fitScreen = args.count >= 4 && args[3] == "screen"
let fitted = fitScreen ? NSScreen.screens.first(where: { $0.backingScaleFactor == 2 }) : nil
let W = fitted.map { Int($0.frame.width) * 2 } ?? 5120
let H = fitted.map { Int($0.frame.height) * 2 } ?? 2880
guard args.count >= 2 else { print("usage: testcard.swift OUT.png [SECONDS]"); exit(2) }

func card() -> CGImage {
    let cs = CGColorSpace(name: CGColorSpace.sRGB)!
    let ctx = CGContext(data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let g = NSGraphicsContext(cgContext: ctx, flipped: true)
    NSGraphicsContext.current = g
    ctx.translateBy(x: 0, y: CGFloat(H)); ctx.scaleBy(x: 1, y: -1)
    NSColor.white.setFill(); NSRect(x: 0, y: 0, width: W, height: H).fill()
    // Text at UI sizes, drawn in pixels at 2x (11 pt = 22 px, ...).
    let lorem = "The quick brown fox jumps over the lazy dog 0123456789 — Settings, Recording, Pause / Resume (⇧⌘R). ilI1| O0o rn m"
    var y: CGFloat = 60
    for pt in [9.0, 11.0, 13.0, 16.0, 24.0] {
        for weight in [NSFont.Weight.regular, .semibold] {
            let a: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: pt * 2, weight: weight), .foregroundColor: NSColor.black]
            (lorem as NSString).draw(at: NSPoint(x: 80, y: y), withAttributes: a)
            y += pt * 2 * 1.5
        }
    }
    let code = """
    fn emit_until(&mut self, t: f64) -> Result<()> {
        let slot = self.slot_ticks(self.next_slot); // CFR grid, 1/600 s ticks
        if self.screen_q.len() > QUEUE_MAX { self.cap(due); }
    }
    """
    let mono: [NSAttributedString.Key: Any] = [.font: NSFont.monospacedSystemFont(ofSize: 26, weight: .regular), .foregroundColor: NSColor(srgbRed: 0.1, green: 0.1, blue: 0.1, alpha: 1)]
    (code as NSString).draw(at: NSPoint(x: 80, y: y + 20), withAttributes: mono)
    y += 260
    // Colored text on colored backgrounds (chroma subsampling shows here).
    let pairs: [(NSColor, NSColor)] = [(.systemRed, .white), (.systemBlue, .white), (.white, .systemBlue), (.systemGreen, .black), (.systemOrange, .white), (.black, .systemYellow)]
    var x: CGFloat = 80
    for (fg, bg) in pairs {
        bg.setFill(); NSRect(x: x, y: y, width: 760, height: 90).fill()
        ("Colored text 13 pt" as NSString).draw(at: NSPoint(x: x + 20, y: y + 26), withAttributes: [.font: NSFont.systemFont(ofSize: 26, weight: .medium), .foregroundColor: fg])
        x += 800
    }
    y += 140
    // 1 px lines and a checkerboard.
    NSColor.black.setFill()
    for i in 0..<60 { NSRect(x: 80 + i * 8, y: Int(y), width: 1, height: 200).fill() }
    for i in 0..<25 { NSRect(x: 600, y: Int(y) + i * 8, width: 800, height: 1).fill() }
    for r in 0..<50 { for c in 0..<100 where (r + c) % 2 == 0 { NSRect(x: 1500 + c * 2, y: Int(y) + r * 2, width: 2, height: 2).fill() } }
    y += 260
    // A smooth gray gradient (banding) and a color gradient.
    let grad = NSGradient(starting: NSColor(white: 0.15, alpha: 1), ending: NSColor(white: 0.35, alpha: 1))!
    grad.draw(in: NSRect(x: 80, y: y, width: 2400, height: 300), angle: 0)
    NSGradient(colors: [.systemPurple, .systemTeal, .systemYellow])!.draw(in: NSRect(x: 2560, y: y, width: 2400, height: 300), angle: 0)
    y += 360
    // A dense paragraph.
    let para = String(repeating: "Rec records the screen, system audio, microphone and webcam into one file that plays in QuickTime. ", count: 18)
    let style = NSMutableParagraphStyle(); style.lineSpacing = 6
    (para as NSString).draw(in: NSRect(x: 80, y: y, width: 4900, height: CGFloat(H) - y - 60),
                            withAttributes: [.font: NSFont.systemFont(ofSize: 26), .foregroundColor: NSColor.darkGray, .paragraphStyle: style])
    NSGraphicsContext.current = nil
    return ctx.makeImage()!
}

let img = card()
let rep = NSBitmapImageRep(cgImage: img)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[1]))
guard args.count >= 3, let secs = Double(args[2]) else { exit(0) }

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
guard let screen = NSScreen.screens.first(where: { $0.backingScaleFactor == 2 && Int($0.frame.width) * 2 == W && Int($0.frame.height) * 2 == H }) else {
    print("no \(W / 2)x\(H / 2) pt display at 2x"); exit(1)
}
let win = NSWindow(contentRect: screen.frame, styleMask: .borderless, backing: .buffered, defer: false)
win.isReleasedWhenClosed = false
let view = NSView(frame: NSRect(origin: .zero, size: screen.frame.size))
view.wantsLayer = true
view.layer!.contents = img
view.layer!.contentsScale = 2
view.layer!.contentsGravity = .topLeft
view.layer!.magnificationFilter = .nearest
win.contentView = view
win.setFrame(screen.frame, display: true)
if args.count >= 5 && args[4] == "front" { win.orderFrontRegardless() } else { win.orderBack(nil) }
print("window \(win.windowNumber)"); fflush(stdout)
RunLoop.main.run(until: Date(timeIntervalSinceNow: secs))
