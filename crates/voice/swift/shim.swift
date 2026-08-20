@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import Speech

private let eventReady: Int32 = 0
private let eventVolatile: Int32 = 1
private let eventFinal: Int32 = 2
private let eventError: Int32 = 3
private let eventEnded: Int32 = 4

public typealias VoiceEventCallback = @convention(c) (
    UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?
) -> Void

@available(macOS 26.0, *)
private enum VoiceSource: Sendable {
    case microphone
    case file(URL)
}

@available(macOS 26.0, *)
private final class AudioFeeder: @unchecked Sendable {
    let stream: AsyncStream<AnalyzerInput>

    private let continuation: AsyncStream<AnalyzerInput>.Continuation
    private let lock = NSLock()
    private var finished = false

    init() {
        (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
    }

    // No explicit bufferStartTime: the analyzer tracks the live input timeline
    // itself, and supplying our own timestamps kept it from ever reporting
    // results or honoring a targeted finalize.
    func feed(_ buffer: AVAudioPCMBuffer) {
        lock.lock()
        defer { lock.unlock() }
        guard !finished else { return }
        continuation.yield(AnalyzerInput(buffer: buffer))
    }

    func finish() {
        lock.lock()
        defer { lock.unlock() }
        guard !finished else { return }
        finished = true
        continuation.finish()
    }
}

@available(macOS 26.0, *)
private actor VoiceCore {
    private let localeIdentifier: String
    private let source: VoiceSource
    private let context: UnsafeMutableRawPointer?
    private let callback: VoiceEventCallback

    private var analyzer: SpeechAnalyzer?
    private var engine: AVAudioEngine?
    private var feeder: AudioFeeder?
    private var analysisTask: Task<Void, Never>?
    private var resultsTask: Task<Void, Never>?
    private var terminal = false
    private var ready = false
    private var stopping = false

    init(
        localeIdentifier: String,
        source: VoiceSource,
        context: UnsafeMutableRawPointer?,
        callback: @escaping VoiceEventCallback
    ) {
        self.localeIdentifier = localeIdentifier
        self.source = source
        self.context = context
        self.callback = callback
    }

    func run() async {
        do {
            let requestedLocale = Locale(identifier: localeIdentifier)
            guard let locale = await SpeechTranscriber.supportedLocale(
                equivalentTo: requestedLocale
            ) else {
                throw VoiceFailure("unsupported speech locale: \(localeIdentifier)")
            }

            let transcriber = SpeechTranscriber(
                locale: locale,
                transcriptionOptions: [],
                reportingOptions: [.volatileResults],
                attributeOptions: []
            )
            let modules: [any SpeechModule] = [transcriber]
            try await installAssets(for: modules)
            guard !stopping else {
                await finishAsEnded(cancel: true)
                return
            }

            let analyzer = SpeechAnalyzer(modules: modules)
            self.analyzer = analyzer
            startResults(from: transcriber)

            switch source {
            case .file(let url):
                try await runFile(url: url, analyzer: analyzer)
            case .microphone:
                try await runMicrophone(analyzer: analyzer, modules: modules)
            }
        } catch {
            if !stopping {
                emitTerminal(kind: eventError, text: error.localizedDescription)
            }
        }
    }

    private func installAssets(for modules: [any SpeechModule]) async throws {
        var status = await AssetInventory.status(forModules: modules)
        guard status != .unsupported else {
            throw VoiceFailure("speech assets are unsupported for \(localeIdentifier)")
        }

        if status != .installed {
            if let request = try await AssetInventory.assetInstallationRequest(
                supporting: modules
            ) {
                try await request.downloadAndInstall()
            }

            status = await AssetInventory.status(forModules: modules)
            while status == .downloading {
                try await Task.sleep(nanoseconds: 100_000_000)
                status = await AssetInventory.status(forModules: modules)
            }
            guard status == .installed else {
                throw VoiceFailure("speech assets could not be installed for \(localeIdentifier)")
            }
        }
    }

    private func startResults(from transcriber: SpeechTranscriber) {
        resultsTask = Task { [weak self] in
            do {
                for try await result in transcriber.results {
                    let text = String(result.text.characters)
                    await self?.emit(
                        kind: result.isFinal ? eventFinal : eventVolatile,
                        text: text
                    )
                }
            } catch {
                await self?.reportFailure(error)
            }
        }
    }

    private func runFile(url: URL, analyzer: SpeechAnalyzer) async throws {
        let file = try AVAudioFile(forReading: url)
        try await analyzer.prepareToAnalyze(in: file.processingFormat)
        emitReady()
        let lastSample = try await analyzer.analyzeSequence(from: file)
        if let lastSample {
            try await analyzer.finalizeAndFinish(through: lastSample)
        } else {
            try await analyzer.finalizeAndFinishThroughEndOfInput()
        }
        await resultsTask?.value
        emitTerminal(kind: eventEnded, text: nil)
    }

    private func runMicrophone(
        analyzer: SpeechAnalyzer,
        modules: [any SpeechModule]
    ) async throws {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        let naturalFormat = input.outputFormat(forBus: 0)
        guard naturalFormat.sampleRate > 0, naturalFormat.channelCount > 0 else {
            throw VoiceFailure("microphone has no usable audio format")
        }
        guard let analysisFormat = await SpeechAnalyzer.bestAvailableAudioFormat(
            compatibleWith: modules,
            considering: naturalFormat
        ) else {
            throw VoiceFailure("no compatible speech audio format")
        }

        let feeder = AudioFeeder()
        self.feeder = feeder
        self.engine = engine
        try await analyzer.prepareToAnalyze(in: analysisFormat)
        analysisTask = Task { [weak self] in
            do {
                try await analyzer.start(inputSequence: feeder.stream)
            } catch {
                await self?.reportFailure(error)
            }
        }

        guard let converter = AVAudioConverter(from: naturalFormat, to: analysisFormat) else {
            throw VoiceFailure("could not create microphone audio converter")
        }
        input.installTap(onBus: 0, bufferSize: 1_024, format: naturalFormat) {
            buffer, _ in
            do {
                // A rate-converting AVAudioConverter may legitimately emit an
                // empty buffer while it primes; skip those instead of failing.
                if let converted = try Self.convert(
                    buffer,
                    with: converter,
                    to: analysisFormat
                ) {
                    feeder.feed(converted)
                }
            } catch {
                Task { [weak self] in
                    await self?.reportFailure(error)
                }
            }
        }
        engine.prepare()
        try engine.start()
        emitReady()
    }

    nonisolated private static func convert(
        _ input: AVAudioPCMBuffer,
        with converter: AVAudioConverter,
        to format: AVAudioFormat
    ) throws -> AVAudioPCMBuffer? {
        let ratio = format.sampleRate / input.format.sampleRate
        let capacity = AVAudioFrameCount(ceil(Double(input.frameLength) * ratio)) + 1
        guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else {
            throw VoiceFailure("could not allocate converted microphone buffer")
        }

        var suppliedInput = false
        var conversionError: NSError?
        let status = converter.convert(to: output, error: &conversionError) { _, state in
            if suppliedInput {
                state.pointee = .noDataNow
                return nil
            }
            suppliedInput = true
            state.pointee = .haveData
            return input
        }
        if let conversionError {
            throw conversionError
        }
        guard status == .haveData || status == .inputRanDry else {
            throw VoiceFailure("microphone audio conversion failed")
        }
        return output.frameLength > 0 ? output : nil
    }

    func stop() async {
        guard !terminal, !stopping else { return }
        stopping = true
        stopCapture()

        do {
            feeder?.finish()
            if let analyzer {
                try await analyzer.finalizeAndFinishThroughEndOfInput()
            }
            await analysisTask?.value
            await resultsTask?.value
            emitTerminal(kind: eventEnded, text: nil)
        } catch {
            emitTerminal(kind: eventError, text: error.localizedDescription)
        }
    }

    func cancel() async {
        guard !terminal else { return }
        stopping = true
        stopCapture()
        feeder?.finish()
        if let analyzer {
            await analyzer.cancelAndFinishNow()
        }
        analysisTask?.cancel()
        resultsTask?.cancel()
        emitTerminal(kind: eventEnded, text: nil)
    }

    private func finishAsEnded(cancel: Bool) async {
        if cancel, let analyzer {
            await analyzer.cancelAndFinishNow()
        }
        emitTerminal(kind: eventEnded, text: nil)
    }

    private func stopCapture() {
        if let engine {
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
        }
        engine = nil
    }

    private func emitReady() {
        guard !terminal, !ready else { return }
        ready = true
        callback(context, eventReady, nil)
    }

    private func reportFailure(_ error: Error) {
        guard !stopping else { return }
        emitTerminal(kind: eventError, text: error.localizedDescription)
    }

    private func emit(kind: Int32, text: String) {
        guard !terminal, ready else { return }
        text.withCString { callback(context, kind, $0) }
    }

    private func emitTerminal(kind: Int32, text: String?) {
        guard !terminal else { return }
        terminal = true
        if let text {
            text.withCString { callback(context, kind, $0) }
        } else {
            callback(context, kind, nil)
        }
    }
}

private struct VoiceFailure: LocalizedError {
    let message: String

    init(_ message: String) {
        self.message = message
    }

    var errorDescription: String? { message }
}

private final class LockedFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false

    func set(_ newValue: Bool) {
        lock.lock()
        value = newValue
        lock.unlock()
    }

    func get() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

@available(macOS 26.0, *)
private final class VoiceSession: @unchecked Sendable {
    let core: VoiceCore

    init(
        locale: String,
        source: VoiceSource,
        context: UnsafeMutableRawPointer?,
        callback: @escaping VoiceEventCallback
    ) {
        core = VoiceCore(
            localeIdentifier: locale,
            source: source,
            context: context,
            callback: callback
        )
        Task { await core.run() }
    }

    func stop() {
        Task { await core.stop() }
    }

    func cancel() {
        Task { await core.cancel() }
    }
}

@_cdecl("voice_is_supported")
public func voiceIsSupported(_ localeCString: UnsafePointer<CChar>?) -> Bool {
    guard #available(macOS 26.0, *), let localeCString else { return false }
    let identifier = String(cString: localeCString)
    let semaphore = DispatchSemaphore(value: 0)
    let supported = LockedFlag()
    Task {
        let result = await SpeechTranscriber.supportedLocale(
            equivalentTo: Locale(identifier: identifier)
        ) != nil
        supported.set(result)
        semaphore.signal()
    }
    semaphore.wait()
    return supported.get()
}

@_cdecl("voice_start")
public func voiceStart(
    _ localeCString: UnsafePointer<CChar>?,
    _ context: UnsafeMutableRawPointer?,
    _ callback: VoiceEventCallback?
) -> UnsafeMutableRawPointer? {
    guard #available(macOS 26.0, *), let localeCString, let callback else { return nil }
    let session = VoiceSession(
        locale: String(cString: localeCString),
        source: .microphone,
        context: context,
        callback: callback
    )
    return Unmanaged.passRetained(session).toOpaque()
}

@_cdecl("voice_start_file")
public func voiceStartFile(
    _ localeCString: UnsafePointer<CChar>?,
    _ pathCString: UnsafePointer<CChar>?,
    _ context: UnsafeMutableRawPointer?,
    _ callback: VoiceEventCallback?
) -> UnsafeMutableRawPointer? {
    guard #available(macOS 26.0, *),
          let localeCString,
          let pathCString,
          let callback
    else { return nil }
    let session = VoiceSession(
        locale: String(cString: localeCString),
        source: .file(URL(fileURLWithPath: String(cString: pathCString))),
        context: context,
        callback: callback
    )
    return Unmanaged.passRetained(session).toOpaque()
}

@_cdecl("voice_stop")
public func voiceStop(_ pointer: UnsafeMutableRawPointer?) {
    guard #available(macOS 26.0, *), let pointer else { return }
    Unmanaged<VoiceSession>.fromOpaque(pointer).takeUnretainedValue().stop()
}

@_cdecl("voice_free")
public func voiceFree(_ pointer: UnsafeMutableRawPointer?) {
    guard #available(macOS 26.0, *), let pointer else { return }
    let unmanaged = Unmanaged<VoiceSession>.fromOpaque(pointer)
    unmanaged.takeUnretainedValue().cancel()
    unmanaged.release()
}
