import UIKit

final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?
    private weak var hostController: GPUIHostViewController?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }

        let controller = GPUIHostViewController()
        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = controller
        window.backgroundColor = UIColor(
            named: "LaunchBackground",
            in: nil,
            compatibleWith: windowScene.traitCollection
        )
        self.window = window
        self.hostController = controller
        window.makeKeyAndVisible()

        DispatchQueue.main.async {
            controller.startGPUIIfNeeded()
        }

        for context in connectionOptions.urlContexts {
            forwardURLToGPUI(context.url)
        }
    }

    func sceneWillEnterForeground(_ scene: UIScene) {
        gpui_ios_lifecycle_foreground()
    }

    func sceneDidBecomeActive(_ scene: UIScene) {
        gpui_ios_lifecycle_active()
        hostController?.resumeFrames()
    }

    func sceneWillResignActive(_ scene: UIScene) {
        gpui_ios_lifecycle_inactive()
    }

    func sceneDidEnterBackground(_ scene: UIScene) {
        gpui_ios_lifecycle_background()
        hostController?.pauseFrames()
    }

    func scene(_ scene: UIScene, openURLContexts URLContexts: Set<UIOpenURLContext>) {
        for context in URLContexts {
            forwardURLToGPUI(context.url)
        }
    }
}
