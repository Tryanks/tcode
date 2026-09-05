import QuartzCore
import UIKit

private final class GPUIKeyboardProxy: UITextView {
    weak var keyEventTarget: GPUIHostView?

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keyEventTarget?.sendPresses(presses, down: true, repeatKey: false)
        super.pressesBegan(presses, with: event)
    }

    override func pressesChanged(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keyEventTarget?.sendPresses(presses, down: true, repeatKey: true)
        super.pressesChanged(presses, with: event)
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keyEventTarget?.sendPresses(presses, down: false, repeatKey: false)
        super.pressesEnded(presses, with: event)
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keyEventTarget?.sendPresses(presses, down: false, repeatKey: false)
        super.pressesCancelled(presses, with: event)
    }
}

final class GPUIHostViewController: UIViewController {
    private var started = false
    private var displayLink: CADisplayLink?
    private var appBackgroundDark: Bool?

    override var preferredStatusBarStyle: UIStatusBarStyle {
        let dark = appBackgroundDark
            ?? (traitCollection.userInterfaceStyle == .dark)
        return dark ? .lightContent : .darkContent
    }

    private var hostView: GPUIHostView {
        view as! GPUIHostView
    }

    override func loadView() {
        view = GPUIHostView(frame: .zero)
        GPUIHostBridge.controller = self
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        startGPUIIfNeeded()
    }

    func startGPUIIfNeeded() {
        guard !started else { return }
        view.layoutIfNeeded()
        guard hostView.attachToGPUI() else { return }

        started = true
        gpui_ios_init()
        tcode_ios_start()

        let link = CADisplayLink(target: self, selector: #selector(renderFrame))
        let maximumFramesPerSecond = hostView.window?.windowScene?.screen.maximumFramesPerSecond
            ?? 60
        link.preferredFrameRateRange = CAFrameRateRange(
            minimum: 30,
            maximum: Float(maximumFramesPerSecond),
            preferred: Float(maximumFramesPerSecond)
        )
        link.add(to: .main, forMode: .common)
        displayLink = link
        gpui_ios_request_frame()
    }

    func resumeFrames() {
        displayLink?.isPaused = false
    }

    func pauseFrames() {
        displayLink?.isPaused = true
    }

    func setAppBackgroundDark(_ dark: Bool) {
        appBackgroundDark = dark
        view.backgroundColor = dark
            ? UIColor(red: 28 / 255, green: 28 / 255, blue: 30 / 255, alpha: 1)
            : UIColor(red: 245 / 255, green: 245 / 255, blue: 247 / 255, alpha: 1)
        setNeedsStatusBarAppearanceUpdate()
    }

    @objc private func renderFrame() {
        gpui_ios_request_frame()
    }

    deinit {
        displayLink?.invalidate()
    }
}

final class GPUIHostView: UIView, UITextViewDelegate {
    private var attached = false
    private var nextTouchIdentifier: UInt64 = 1
    private var touchIdentifiers: [ObjectIdentifier: UInt64] = [:]
    private var resettingKeyboardProxy = false
    private var hasForwardedMarkedText = false
    private let keyboardProxy = GPUIKeyboardProxy(
        frame: CGRect(x: -2, y: -2, width: 1, height: 1)
    )

    override class var layerClass: AnyClass {
        CAMetalLayer.self
    }

    private var metalLayer: CAMetalLayer {
        layer as! CAMetalLayer
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = true
        backgroundColor = UIColor(red: 245 / 255, green: 245 / 255, blue: 247 / 255, alpha: 1)
        isMultipleTouchEnabled = true
        contentScaleFactor = traitCollection.displayScale

        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = true
        metalLayer.contentsScale = contentScaleFactor
        metalLayer.presentsWithTransaction = false

        keyboardProxy.delegate = self
        keyboardProxy.keyEventTarget = self
        keyboardProxy.backgroundColor = .clear
        keyboardProxy.textColor = .clear
        keyboardProxy.tintColor = .clear
        keyboardProxy.alpha = 0.01
        keyboardProxy.isAccessibilityElement = false
        keyboardProxy.autocorrectionType = .no
        keyboardProxy.autocapitalizationType = .none
        keyboardProxy.spellCheckingType = .no
        keyboardProxy.smartDashesType = .no
        keyboardProxy.smartQuotesType = .no
        keyboardProxy.smartInsertDeleteType = .no
        addSubview(keyboardProxy)

        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardFrameChanged(_:)),
            name: UIResponder.keyboardWillChangeFrameNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardFrameChanged(_:)),
            name: UIResponder.keyboardWillHideNotification,
            object: nil
        )
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) {
            (view: GPUIHostView, _: UITraitCollection) in
            guard view.attached else { return }
            gpui_ios_appearance_changed(view.traitCollection.userInterfaceStyle == .dark ? 1 : 0)
        }
        GPUIHostBridge.view = self
    }

    required init?(coder: NSCoder) {
        fatalError("GPUIHostView must be created programmatically")
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
        if attached {
            gpui_ios_detach_view(Unmanaged.passUnretained(self).toOpaque())
        }
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        guard let screen = window?.windowScene?.screen else { return }
        contentScaleFactor = screen.scale
        metalLayer.contentsScale = screen.scale
        if attached {
            gpui_ios_scale_factor_changed(Float(screen.scale))
        }
    }

    @discardableResult
    func attachToGPUI() -> Bool {
        if attached { return true }
        guard window != nil, bounds.width > 0, bounds.height > 0 else { return false }

        let result = gpui_ios_attach_view(
            Unmanaged.passUnretained(self).toOpaque(),
            Float(bounds.width),
            Float(bounds.height),
            Float(contentScaleFactor),
            traitCollection.userInterfaceStyle == .dark ? 1 : 0
        )
        attached = result != 0
        if attached {
            sendGeometry()
            sendSafeArea()
        }
        return attached
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        keyboardProxy.frame = CGRect(x: -2, y: -2, width: 1, height: 1)
        metalLayer.contentsScale = contentScaleFactor
        metalLayer.drawableSize = CGSize(
            width: bounds.width * contentScaleFactor,
            height: bounds.height * contentScaleFactor
        )
        if attached {
            sendGeometry()
        }
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        if attached {
            sendSafeArea()
        }
    }

    private func sendGeometry() {
        gpui_ios_resize(
            Float(bounds.width),
            Float(bounds.height),
            Float(contentScaleFactor)
        )
    }

    private func sendSafeArea() {
        gpui_ios_safe_area_changed(
            Float(safeAreaInsets.top),
            Float(safeAreaInsets.right),
            Float(safeAreaInsets.bottom),
            Float(safeAreaInsets.left)
        )
    }

    @objc private func keyboardFrameChanged(_ notification: Notification) {
        guard let frame = notification.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect,
              notification.name != UIResponder.keyboardWillHideNotification
        else {
            gpui_ios_keyboard_frame_changed(0)
            return
        }

        let localFrame = convert(frame, from: nil)
        let covered = bounds.intersection(localFrame)
        gpui_ios_keyboard_frame_changed(Float(covered.isNull ? 0 : covered.height))
    }

    func showKeyboard() {
        keyboardProxy.becomeFirstResponder()
    }

    func hideKeyboard() {
        keyboardProxy.resignFirstResponder()
    }

    func configureTextInput(
        autocorrect: Bool,
        autocapitalize: UInt32,
        suggestions: Bool,
        inputAction: UInt32
    ) {
        keyboardProxy.autocorrectionType = autocorrect ? .yes : .no
        keyboardProxy.spellCheckingType = suggestions ? .yes : .no
        keyboardProxy.autocapitalizationType = switch autocapitalize {
        case 1: .words
        case 2: .sentences
        case 3: .allCharacters
        default: .none
        }
        keyboardProxy.returnKeyType = switch inputAction {
        case 1: .default
        case 2: .done
        case 3: .go
        case 4: .next
        case 5: .continue
        case 6: .search
        case 7: .send
        default: .default
        }
        if keyboardProxy.isFirstResponder {
            keyboardProxy.reloadInputViews()
        }
    }

    func textViewDidChange(_ textView: UITextView) {
        guard !resettingKeyboardProxy else { return }

        if let markedRange = textView.markedTextRange,
           let markedText = textView.text(in: markedRange)
        {
            hasForwardedMarkedText = true
            var selectionStart = markedText.utf16.count
            var selectionLength = 0
            if let selected = textView.selectedTextRange {
                selectionStart = max(
                    0,
                    textView.offset(from: markedRange.start, to: selected.start)
                )
                selectionLength = max(
                    0,
                    textView.offset(from: selected.start, to: selected.end)
                )
            }
            withUTF8(markedText) { bytes, length in
                gpui_ios_set_marked_text(
                    bytes,
                    length,
                    selectionStart,
                    selectionLength
                )
            }
            return
        }

        let committed = textView.text ?? ""
        if !committed.isEmpty {
            withUTF8(committed) { bytes, length in
                gpui_ios_insert_text(bytes, length)
            }
        } else if hasForwardedMarkedText {
            gpui_ios_unmark_text()
        }
        hasForwardedMarkedText = false
        resettingKeyboardProxy = true
        textView.text = ""
        resettingKeyboardProxy = false
    }

    func textView(
        _ textView: UITextView,
        shouldChangeTextIn range: NSRange,
        replacementText text: String
    ) -> Bool {
        if text == "\n" {
            sendKey(name: "enter", character: "\n", modifiers: 0, down: true, repeatKey: false)
            sendKey(name: "enter", character: "\n", modifiers: 0, down: false, repeatKey: false)
            return false
        }
        if text.isEmpty, textView.markedTextRange == nil, textView.text.isEmpty {
            gpui_ios_delete_backward()
            return false
        }
        return true
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesBegan(touches, with: event)
        for touch in touches {
            touchIdentifiers[ObjectIdentifier(touch)] = nextTouchIdentifier
            nextTouchIdentifier &+= 1
        }
        sendTouches(touches, event: event, phase: .began)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesMoved(touches, with: event)
        sendTouches(touches, event: event, phase: .moved)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesEnded(touches, with: event)
        sendTouches(touches, event: event, phase: .ended)
        for touch in touches {
            touchIdentifiers.removeValue(forKey: ObjectIdentifier(touch))
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesCancelled(touches, with: event)
        sendTouches(touches, event: event, phase: .cancelled)
        for touch in touches {
            touchIdentifiers.removeValue(forKey: ObjectIdentifier(touch))
        }
    }

    private enum ForwardedTouchPhase {
        case began
        case moved
        case ended
        case cancelled
    }

    private func sendTouches(
        _ touches: Set<UITouch>,
        event: UIEvent?,
        phase: ForwardedTouchPhase
    ) {
        var serialized = touches.compactMap { touch -> GpuiIosTouch? in
            guard let identifier = touchIdentifiers[ObjectIdentifier(touch)] else { return nil }
            let position = touch.location(in: self)
            let predicted = phase == .moved ? event?.predictedTouches(for: touch)?.last : nil
            let predictedPosition = predicted?.location(in: self) ?? position
            let maximumForce = touch.maximumPossibleForce
            let normalizedForce = maximumForce > 0 ? touch.force / maximumForce : 0
            return GpuiIosTouch(
                identifier: identifier,
                x: position.x,
                y: position.y,
                predicted_x: predictedPosition.x,
                predicted_y: predictedPosition.y,
                force: Float(normalizedForce),
                has_prediction: predicted == nil ? 0 : 1,
                has_force: maximumForce > 0 ? 1 : 0
            )
        }
        serialized.sort { $0.identifier < $1.identifier }
        serialized.withUnsafeBufferPointer { buffer in
            switch phase {
            case .began:
                gpui_ios_touches_began(buffer.baseAddress, buffer.count)
            case .moved:
                gpui_ios_touches_moved(buffer.baseAddress, buffer.count)
            case .ended:
                gpui_ios_touches_ended(buffer.baseAddress, buffer.count)
            case .cancelled:
                gpui_ios_touches_cancelled(buffer.baseAddress, buffer.count)
            }
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesBegan(presses, with: event)
        sendPresses(presses, down: true, repeatKey: false)
    }

    override func pressesChanged(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesChanged(presses, with: event)
        sendPresses(presses, down: true, repeatKey: true)
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesEnded(presses, with: event)
        sendPresses(presses, down: false, repeatKey: false)
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesCancelled(presses, with: event)
        sendPresses(presses, down: false, repeatKey: false)
    }

    fileprivate func sendPresses(_ presses: Set<UIPress>, down: Bool, repeatKey: Bool) {
        for press in presses {
            guard let key = press.key else { continue }
            let name = gpuiKeyName(for: key)
            sendKey(
                name: name,
                character: gpuiCharacter(for: key, name: name),
                modifiers: modifierBits(key.modifierFlags),
                down: down,
                repeatKey: repeatKey
            )
        }
    }
}

private func gpuiKeyName(for key: UIKey) -> String {
    switch key.charactersIgnoringModifiers {
    case UIKeyCommand.inputUpArrow: "up"
    case UIKeyCommand.inputDownArrow: "down"
    case UIKeyCommand.inputLeftArrow: "left"
    case UIKeyCommand.inputRightArrow: "right"
    case UIKeyCommand.inputEscape: "escape"
    case UIKeyCommand.inputDelete: "backspace"
    case "\r", "\n": "enter"
    case "\t": "tab"
    case " ": "space"
    default: key.charactersIgnoringModifiers.lowercased()
    }
}

private func gpuiCharacter(for key: UIKey, name: String) -> String {
    switch name {
    case "up", "down", "left", "right", "escape", "backspace", "tab": ""
    case "enter": "\n"
    case "space": " "
    default: key.characters
    }
}

private func modifierBits(_ flags: UIKeyModifierFlags) -> UInt32 {
    var bits: UInt32 = 0
    if flags.contains(.shift) { bits |= 1 }
    if flags.contains(.control) { bits |= 2 }
    if flags.contains(.alternate) { bits |= 4 }
    if flags.contains(.command) { bits |= 8 }
    return bits
}

private func sendKey(
    name: String,
    character: String,
    modifiers: UInt32,
    down: Bool,
    repeatKey: Bool
) {
    withUTF8(name) { keyBytes, keyLength in
        withUTF8(character) { characterBytes, characterLength in
            gpui_ios_key_event(
                keyBytes,
                keyLength,
                characterBytes,
                characterLength,
                modifiers,
                down ? 1 : 0,
                repeatKey ? 1 : 0
            )
        }
    }
}
