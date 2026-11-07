//! Notification Center. `UNUserNotificationCenter` works only in a bundled
//! app (it aborts in a bare binary, which is what `cargo run` starts), so the
//! notifier exists only in one.

use async_channel::{Receiver, Sender, unbounded};
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSArray, NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use tcode_client::host::SystemNotifier;

struct DelegateIvars {
    opened: Sender<String>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TcodeNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            let thread_id = response.notification().request().identifier().to_string();
            let _ = self.ivars().opened.try_send(thread_id);
            completion.call(());
        }
    }
);

impl Delegate {
    fn new(opened: Sender<String>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { opened });
        // SAFETY: NSObject's init, on a class whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct MacNotifier {
    opened: Receiver<String>,
    /// The center holds its delegate weakly.
    _delegate: Retained<Delegate>,
}

impl MacNotifier {
    pub(crate) fn new() -> Option<Self> {
        if !NSBundle::mainBundle()
            .bundlePath()
            .to_string()
            .ends_with(".app")
        {
            return None;
        }
        let (sender, opened) = unbounded();
        let delegate = Delegate::new(sender);
        UNUserNotificationCenter::currentNotificationCenter()
            .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Some(Self {
            opened,
            _delegate: delegate,
        })
    }
}

impl SystemNotifier for MacNotifier {
    fn post(&self, thread_id: &str, title: &str, body: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(thread_id),
            &content,
            None,
        );
        // The system asks the user only the first time; every later call
        // answers at once with what they chose.
        let granted = RcBlock::new(move |granted: Bool, _: *mut NSError| {
            if granted.as_bool() {
                UNUserNotificationCenter::currentNotificationCenter()
                    .addNotificationRequest_withCompletionHandler(&request, None);
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &granted,
            );
    }

    fn withdraw(&self, thread_id: &str) {
        let identifiers = NSArray::from_retained_slice(&[NSString::from_str(thread_id)]);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
        center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
    }

    fn opened(&self) -> Receiver<String> {
        self.opened.clone()
    }
}
