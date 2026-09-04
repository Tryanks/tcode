import UIKit

enum GPUIHostBridge {
    static weak var view: GPUIHostView?
    static weak var controller: GPUIHostViewController?
}

func withUTF8(
    _ string: String,
    _ body: (UnsafePointer<UInt8>?, Int) -> Void
) {
    let bytes = Array(string.utf8)
    bytes.withUnsafeBufferPointer { buffer in
        body(buffer.baseAddress, buffer.count)
    }
}

func forwardURLToGPUI(_ url: URL) {
    withUTF8(url.absoluteString) { bytes, length in
        gpui_ios_open_url_received(bytes, length)
    }
}

@_cdecl("gpui_ios_host_log")
public func gpuiHostLog(_ level: UInt32, _ bytes: UnsafePointer<UInt8>?, _ length: Int) {
    guard let bytes,
          let message = String(
              bytes: UnsafeBufferPointer(start: bytes, count: length),
              encoding: .utf8
          )
    else { return }
    NSLog("GPUI[%u] %@", level, message)
}

@_cdecl("gpui_ios_host_schedule_frame")
public func gpuiHostScheduleFrame() {
    GPUIHostBridge.view?.setNeedsDisplay()
}

@_cdecl("gpui_ios_host_show_keyboard")
public func gpuiHostShowKeyboard() {
    GPUIHostBridge.view?.showKeyboard()
}

@_cdecl("gpui_ios_host_hide_keyboard")
public func gpuiHostHideKeyboard() {
    GPUIHostBridge.view?.hideKeyboard()
}

@_cdecl("gpui_ios_host_configure_text_input")
public func gpuiHostConfigureTextInput(
    _ autocorrect: UInt8,
    _ autocapitalize: UInt32,
    _ suggestions: UInt8,
    _ inputAction: UInt32
) {
    GPUIHostBridge.view?.configureTextInput(
        autocorrect: autocorrect != 0,
        autocapitalize: autocapitalize,
        suggestions: suggestions != 0,
        inputAction: inputAction
    )
}

@_cdecl("gpui_ios_host_open_url")
public func gpuiHostOpenURL(_ bytes: UnsafePointer<UInt8>?, _ length: Int) {
    guard let bytes,
          let value = String(bytes: UnsafeBufferPointer(start: bytes, count: length), encoding: .utf8),
          let url = URL(string: value)
    else { return }
    UIApplication.shared.open(url)
}

@_cdecl("gpui_ios_host_clipboard_text_length")
public func gpuiHostClipboardTextLength() -> Int {
    UIPasteboard.general.string?.utf8.count ?? 0
}

@_cdecl("gpui_ios_host_read_clipboard")
public func gpuiHostReadClipboard(_ destination: UnsafeMutablePointer<UInt8>?, _ capacity: Int) -> Int {
    guard let destination, capacity > 0, let string = UIPasteboard.general.string else { return 0 }
    let bytes = Array(string.utf8)
    let count = min(capacity, bytes.count)
    bytes.withUnsafeBufferPointer { buffer in
        if let source = buffer.baseAddress {
            destination.update(from: source, count: count)
        }
    }
    return count
}

@_cdecl("gpui_ios_host_write_clipboard")
public func gpuiHostWriteClipboard(_ bytes: UnsafePointer<UInt8>?, _ length: Int) {
    guard let bytes,
          let value = String(bytes: UnsafeBufferPointer(start: bytes, count: length), encoding: .utf8)
    else { return }
    UIPasteboard.general.string = value
}
