//! Cross-platform app-activation events for permission rechecks.

pub(crate) struct AppActivationObserver {
    #[cfg(all(feature = "desktop", target_os = "macos"))]
    center: objc2::rc::Retained<objc2_foundation::NSNotificationCenter>,
    #[cfg(all(feature = "desktop", target_os = "macos"))]
    token:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

#[cfg(all(feature = "desktop", target_os = "macos"))]
impl Drop for AppActivationObserver {
    fn drop(&mut self) {
        // SAFETY: `token` was returned by this notification center and remains
        // retained for the lifetime of the observer.
        let protocol: &objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol> =
            &self.token;
        let observer = AsRef::<objc2::runtime::AnyObject>::as_ref(protocol);
        unsafe { self.center.removeObserver(observer) };
    }
}

pub(crate) fn observe() -> (AppActivationObserver, async_channel::Receiver<()>) {
    let (sender, receiver) = async_channel::unbounded();

    #[cfg(all(feature = "desktop", target_os = "macos"))]
    {
        use std::ptr::NonNull;

        use block2::RcBlock;
        use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
        use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue};

        let block: RcBlock<dyn Fn(NonNull<NSNotification>)> = RcBlock::new(move |_notification| {
            let _ = sender.try_send(());
        });
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: AppKit posts this notification on the main thread, the block
        // is retained by the notification center, and `queue` keeps delivery on
        // the main operation queue.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidBecomeActiveNotification),
                None,
                Some(&queue),
                &block,
            )
        };
        (AppActivationObserver { center, token }, receiver)
    }

    #[cfg(not(all(feature = "desktop", target_os = "macos")))]
    {
        drop(sender);
        (AppActivationObserver {}, receiver)
    }
}
