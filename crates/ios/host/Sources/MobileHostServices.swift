import AVFoundation
import UIKit

private struct MobileHostServiceError: LocalizedError {
    let message: String

    init(_ message: String) {
        self.message = message
    }

    var errorDescription: String? { message }
}

@discardableResult
private func copyUTF8(
    _ value: String,
    to destination: UnsafeMutablePointer<UInt8>?,
    capacity: Int
) -> Int {
    let bytes = Array(value.utf8)
    guard let destination, capacity >= bytes.count else { return bytes.count }
    bytes.withUnsafeBufferPointer { buffer in
        if let source = buffer.baseAddress, !bytes.isEmpty {
            destination.update(from: source, count: bytes.count)
        }
    }
    return bytes.count
}

private func topPresenter() -> UIViewController? {
    var presenter = GPUIHostBridge.controller
        ?? GPUIHostBridge.view?.window?.rootViewController
    while let presented = presenter?.presentedViewController {
        presenter = presented
    }
    return presenter
}

@_cdecl("tcode_ios_host_device_name")
public func tcodeIosHostDeviceName(
    _ destination: UnsafeMutablePointer<UInt8>?,
    _ capacity: Int
) -> Int {
    copyUTF8(UIDevice.current.name, to: destination, capacity: capacity)
}

@_cdecl("tcode_ios_host_start_camera_scan")
public func tcodeIosHostStartCameraScan(_ requestId: UInt64) {
    func begin() {
        guard let device = AVCaptureDevice.default(
            .builtInWideAngleCamera,
            for: .video,
            position: .back
        ) ?? AVCaptureDevice.default(for: .video) else {
            completeCamera(
                requestId,
                result: .failure(MobileHostServiceError("此设备没有可用的相机"))
            )
            return
        }
        guard let presenter = topPresenter() else {
            completeCamera(
                requestId,
                result: .failure(MobileHostServiceError("无法显示相机扫描界面"))
            )
            return
        }
        let scanner = QRScannerViewController(device: device) { result in
            completeCamera(requestId, result: result)
        }
        scanner.modalPresentationStyle = .fullScreen
        presenter.present(scanner, animated: true)
    }

    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
        begin()
    case .notDetermined:
        AVCaptureDevice.requestAccess(for: .video) { granted in
            DispatchQueue.main.async {
                if granted {
                    begin()
                } else {
                    completeCamera(
                        requestId,
                        result: .failure(MobileHostServiceError("相机权限被拒绝"))
                    )
                }
            }
        }
    default:
        completeCamera(
            requestId,
            result: .failure(MobileHostServiceError("相机权限被拒绝"))
        )
    }
}

private func completeCamera(_ requestId: UInt64, result: Result<String, Error>) {
    switch result {
    case .success(let value):
        withUTF8(value) { valueBytes, valueLength in
            tcode_ios_camera_scan_completed(
                requestId,
                valueBytes,
                valueLength,
                nil,
                0
            )
        }
    case .failure(let error):
        withUTF8(error.localizedDescription) { errorBytes, errorLength in
            tcode_ios_camera_scan_completed(
                requestId,
                nil,
                0,
                errorBytes,
                errorLength
            )
        }
    }
}

private final class QRScannerViewController: UIViewController,
    AVCaptureMetadataOutputObjectsDelegate
{
    private let device: AVCaptureDevice
    private let completion: (Result<String, Error>) -> Void
    private let session = AVCaptureSession()
    private let previewLayer = AVCaptureVideoPreviewLayer()
    private var finished = false

    init(
        device: AVCaptureDevice,
        completion: @escaping (Result<String, Error>) -> Void
    ) {
        self.device = device
        self.completion = completion
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("QRScannerViewController must be created programmatically")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black

        previewLayer.videoGravity = .resizeAspectFill
        previewLayer.session = session
        view.layer.addSublayer(previewLayer)

        let frame = UIView()
        frame.isUserInteractionEnabled = false
        frame.layer.cornerRadius = 24
        frame.layer.borderWidth = 3
        frame.layer.borderColor = UIColor.systemBlue.cgColor
        frame.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(frame)

        let cancel = UIButton(type: .system)
        cancel.setTitle("取消", for: .normal)
        cancel.setTitleColor(.white, for: .normal)
        cancel.titleLabel?.font = .systemFont(ofSize: 17, weight: .semibold)
        cancel.backgroundColor = UIColor.black.withAlphaComponent(0.45)
        cancel.layer.cornerRadius = 18
        cancel.addTarget(self, action: #selector(cancelScan), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(cancel)

        let label = UILabel()
        label.text = "将二维码置于取景框内"
        label.textColor = .white
        label.font = .systemFont(ofSize: 17, weight: .semibold)
        label.textAlignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)

        NSLayoutConstraint.activate([
            frame.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            frame.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            frame.widthAnchor.constraint(equalTo: view.widthAnchor, multiplier: 0.78),
            frame.heightAnchor.constraint(equalTo: frame.widthAnchor),
            cancel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 12),
            cancel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            cancel.widthAnchor.constraint(equalToConstant: 72),
            cancel.heightAnchor.constraint(equalToConstant: 44),
            label.bottomAnchor.constraint(equalTo: frame.topAnchor, constant: -20),
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
        ])

        configureSession()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        previewLayer.frame = view.bounds
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if session.isRunning {
            DispatchQueue.global(qos: .userInitiated).async { [session] in
                session.stopRunning()
            }
        }
    }

    private func configureSession() {
        do {
            let input = try AVCaptureDeviceInput(device: device)
            let output = AVCaptureMetadataOutput()
            session.beginConfiguration()
            guard session.canAddInput(input), session.canAddOutput(output) else {
                session.commitConfiguration()
                finish(.failure(MobileHostServiceError("无法启动二维码扫描")))
                return
            }
            session.addInput(input)
            session.addOutput(output)
            output.setMetadataObjectsDelegate(self, queue: .main)
            output.metadataObjectTypes = [.qr]
            session.commitConfiguration()
            DispatchQueue.global(qos: .userInitiated).async { [session] in
                session.startRunning()
            }
        } catch {
            finish(.failure(error))
        }
    }

    @objc private func cancelScan() {
        finish(.failure(MobileHostServiceError("已取消扫描")))
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard let object = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
              let value = object.stringValue
        else { return }
        finish(.success(value))
    }

    private func finish(_ result: Result<String, Error>) {
        guard !finished else { return }
        finished = true
        dismiss(animated: true) { [completion] in completion(result) }
    }
}
