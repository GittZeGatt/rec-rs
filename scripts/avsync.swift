// A/V sync as AVFoundation (QuickTime's engine) sees it: decode a
// flash + beep recording with AVAssetReader and print the offset between
// each white flash and the nearest beep onset, in ms (audio minus video).
// Usage: swift scripts/avsync.swift FILE
import AVFoundation

let url = URL(fileURLWithPath: CommandLine.arguments[1])
let asset = AVURLAsset(url: url)
let sem = DispatchSemaphore(value: 0)
var vtrack: AVAssetTrack?, atrack: AVAssetTrack?
Task {
    vtrack = try? await asset.loadTracks(withMediaType: .video).first
    atrack = try? await asset.loadTracks(withMediaType: .audio).first
    sem.signal()
}
sem.wait()
guard let vt = vtrack, let at = atrack else { print("missing tracks"); exit(1) }
let reader = try! AVAssetReader(asset: asset)
let vout = AVAssetReaderTrackOutput(track: vt, outputSettings: [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA])
let aout = AVAssetReaderTrackOutput(track: at, outputSettings: [AVFormatIDKey: kAudioFormatLinearPCM, AVLinearPCMBitDepthKey: 32, AVLinearPCMIsFloatKey: true, AVLinearPCMIsNonInterleaved: false, AVNumberOfChannelsKey: 1, AVSampleRateKey: 48000])
reader.add(vout); reader.add(aout)
reader.startReading()
var flashes: [Double] = [], onsets: [Double] = []
var prev = 0.0
while let sb = vout.copyNextSampleBuffer() {
    guard let pb = CMSampleBufferGetImageBuffer(sb) else { continue }
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), bpr = CVPixelBufferGetBytesPerRow(pb)
    let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
    var sum = 0.0, n = 0.0
    for y in stride(from: 0, to: h, by: 8) { for x in stride(from: 0, to: w, by: 8) { sum += Double(base[y * bpr + x * 4 + 1]); n += 1 } }
    CVPixelBufferUnlockBaseAddress(pb, .readOnly)
    let l = sum / n
    if l > 180 && prev < 140 { flashes.append(CMSampleBufferGetPresentationTimeStamp(sb).seconds) }
    prev = l
}
var last = -1e9
while let sb = aout.copyNextSampleBuffer() {
    let t0 = CMSampleBufferGetPresentationTimeStamp(sb).seconds
    guard let bb = CMSampleBufferGetDataBuffer(sb) else { continue }
    var len = 0; var ptr: UnsafeMutablePointer<CChar>?
    CMBlockBufferGetDataPointer(bb, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &len, dataPointerOut: &ptr)
    let n = len / 4
    ptr!.withMemoryRebound(to: Float.self, capacity: n) { f in
        for i in 0..<n where abs(f[i]) > 0.1 {
            let t = t0 + Double(i) / 48000
            if t - last > 0.2 { onsets.append(t) }
            last = t
        }
    }
}
var offs: [Double] = []
for f in flashes {
    if let b = onsets.min(by: { abs($0 - f) < abs($1 - f) }), abs(b - f) < 0.2 { offs.append((b - f) * 1000) }
}
let mean = offs.reduce(0, +) / Double(max(offs.count, 1))
print(String(format: "AVFoundation: %d flashes, %d beeps, %d paired, offset mean %+.1f ms, range [%+.1f, %+.1f]", flashes.count, onsets.count, offs.count, mean, offs.min() ?? 0, offs.max() ?? 0))
