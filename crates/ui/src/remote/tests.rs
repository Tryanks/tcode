use super::*;
use gpui::{AppContext as _, Render, TestAppContext};
use tcode_client::host::FileFailure;

#[cfg(feature = "remote-hosting")]
struct PairingProbe(Entity<RemotePanel>);

#[cfg(feature = "remote-hosting")]
impl Render for PairingProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The browser is signed in with the machine that served it: its Hosts
/// page lists that machine and nothing that would add or re-pair one.
#[gpui::test]
fn a_fixed_machine_client_has_no_way_to_add_one(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    struct Browser;
    impl ClientHost for Browser {
        fn device_name(&self) -> String {
            "Safari".into()
        }
        fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
            Ok("browser".into())
        }
        fn device_platform(&self) -> Option<String> {
            None
        }
        fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
            Ok(vec![PairedHost {
                host_id: "served-by".into(),
                name: "Build server".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: Vec::new(),
                last_connected_unix: None,
                group_id: None,
                space_id: None,
                space_name: None,
            }])
        }
        fn save_hosts(&self, _: &[PairedHost]) -> Result<(), FileFailure> {
            Ok(())
        }
        fn last_host_id(&self) -> Option<String> {
            Some("served-by".into())
        }
        fn set_last_host_id(&self, _: Option<&str>) {}
        fn fixed_machine(&self) -> bool {
            true
        }
        fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
            unreachable!("the page only lists the machine")
        }
    }
    struct HostsProbe(Entity<RemotePanel>);
    impl Render for HostsProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(
                self.0
                    .update(cx, |panel, cx| panel.render_hosts(window, cx)),
            )
        }
    }
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_global(ClientAttachment::new(Rc::new(Browser), false, |_, _, _| {})));
    let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
        let state = cx.new(|_| WindowState::default());
        HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
    });
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    assert!(
        cx.debug_bounds("host-served-by").is_some(),
        "the serving machine is listed"
    );
    assert!(
        cx.debug_bounds("hosts-add-machine").is_none(),
        "a browser has no way to add a machine"
    );
}

/// Saved machines that cannot be read are not "no machines": the list
/// says the file could not be read, and why.
#[gpui::test]
fn unreadable_saved_machines_say_so_on_the_hosts_page(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = crate::store::tests::scratch_root("hosts-page-unreadable");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hosts.json"), br#"[{"host_id": 7}]"#).unwrap();
    let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
    struct HostsProbe(Entity<RemotePanel>);
    impl Render for HostsProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(
                self.0
                    .update(cx, |panel, cx| panel.render_hosts(window, cx)),
            )
        }
    }
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_global(ClientAttachment::new(client, false, |_, _, _| {})));
    let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
        let state = cx.new(|_| WindowState::default());
        HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
    });
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("hosts-unreadable").is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// A scanned link that opens no path to the machine: the Hosts page asks
/// for the machine's address under the failure, and the retry sends the
/// same invitation with it.
#[gpui::test]
fn a_link_that_found_no_path_asks_for_the_address_and_retries_with_it(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    use std::net::IpAddr;
    use tcode_client::pairing::PairInvite;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    #[derive(Default)]
    struct Phone {
        attempts: RefCell<Vec<(PairInvite, Option<IpAddr>)>>,
        hosts: RefCell<Vec<PairedHost>>,
    }
    impl ClientHost for Phone {
        fn device_name(&self) -> String {
            "phone".into()
        }
        fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
            Ok("phone".into())
        }
        fn device_platform(&self) -> Option<String> {
            None
        }
        fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
            Ok(self.hosts.borrow().clone())
        }
        fn save_hosts(&self, hosts: &[PairedHost]) -> Result<(), FileFailure> {
            *self.hosts.borrow_mut() = hosts.to_vec();
            Ok(())
        }
        fn last_host_id(&self) -> Option<String> {
            None
        }
        fn set_last_host_id(&self, _: Option<&str>) {}
        fn supports_qr(&self) -> bool {
            true
        }
        fn pair(
            &self,
            invite: PairInvite,
            address: Option<IpAddr>,
        ) -> tcode_client::host::HostFuture<'_, Result<PairedHost, tcode_client::pairing::PairError>>
        {
            self.attempts.borrow_mut().push((invite.clone(), address));
            Box::pin(async move {
                let reached: Option<IpAddr> = "192.168.1.20".parse().ok();
                if address == reached {
                    Ok(invite.paired("Studio".into()))
                } else {
                    Err(tcode_client::pairing::PairError::Unreachable(
                        "No addressing information available".into(),
                    ))
                }
            })
        }
        fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
            unreachable!("the switch is the attachment owner's")
        }
    }
    struct HostsProbe(Entity<RemotePanel>);
    impl Render for HostsProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(
                self.0
                    .update(cx, |panel, cx| panel.render_hosts(window, cx)),
            )
        }
    }
    let phone = Rc::new(Phone::default());
    cx.update(crate::test_client::init);
    cx.update(|cx| cx.set_global(ClientAttachment::new(phone.clone(), false, |_, _, _| {})));
    let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
        let state = cx.new(|_| WindowState::default());
        HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
    });
    let panel = window.update(cx, |probe, _, _| probe.0.clone()).unwrap();
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    };
    let invite = PairInvite {
        host_id: "ab".repeat(32),
        secret: "AAECAwQFBgcICQoLDA0ODw".into(),
        space: None,
        group: None,
        traverse: Vec::new(),
        relay: None,
        port: 47420,
    };
    let link = tcode_client::pairing::pair_url(&invite);
    // What a scan does with the link it read.
    cx.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            assert!(panel.form.fill_invite(&link, window, cx));
            panel.submit(window, cx);
        });
    });
    draw(cx);
    assert_eq!(*phone.attempts.borrow(), [(invite.clone(), None)]);
    assert!(
        cx.debug_bounds("hosts-pair-address").is_some(),
        "no path: the page asks for the address"
    );
    assert!(
        cx.debug_bounds("hosts-pair-error").is_none()
            && cx.debug_bounds("hosts-pair-address-error").is_none(),
        "the prompt stands in for the transport's cause"
    );
    let retry = |address: &'static str, cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel
                    .form
                    .address
                    .update(cx, |state, cx| state.set_value(address, window, cx));
                panel.submit(window, cx);
            });
        });
        draw(cx);
    };
    retry("192.168.1.99", cx);
    assert!(
        cx.debug_bounds("hosts-pair-address-error").is_some(),
        "a typed address that found no path says so in the prompt"
    );
    assert!(cx.debug_bounds("hosts-pair-error").is_none());
    retry("192.168.1.20", cx);
    assert_eq!(
        phone.attempts.borrow()[2],
        (invite.clone(), Some("192.168.1.20".parse().unwrap())),
        "the same invitation, with the typed address"
    );
    assert_eq!(
        phone.load_hosts().unwrap(),
        [invite.paired("Studio".into())]
    );
    assert!(cx.debug_bounds("hosts-pair-address").is_none());
}

/// The Hosts page is where this machine offers a connection: with hosting
/// off it points at the setting; hosting and accepting devices, it shows
/// the invitation itself, and the expiry when that runs out.
#[cfg(feature = "remote-hosting")]
#[gpui::test]
fn the_hosts_page_offers_this_machines_invitation(cx: &mut TestAppContext) {
    use gpui::BorrowAppContext as _;
    use tcode_client::HostLink;
    use tcode_core::settings::TraverseSetting;
    use tcode_traverse::HostMux;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    // The machine's host runs on its own threads and tells the window
    // of hosting changes from there, as it does in the app.
    cx.executor().allow_parking();
    let root = crate::store::tests::scratch_root("hosts-invitation");
    std::fs::create_dir_all(&root).unwrap();
    let (to_host, _host_rx) = async_channel::unbounded::<String>();
    let (_host_tx, from_host) = async_channel::unbounded::<String>();
    let (attachments, _attachment_rx) = async_channel::unbounded();
    let mux = HostMux::new(attachments, from_host.clone());
    cx.update(crate::test_client::init);
    cx.update(|cx| {
        cx.set_global(RemoteController::new(
            mux.clone(),
            root.clone(),
            None,
            HostLink::new(to_host, from_host),
            tcode_core::settings::Settings::default(),
        ));
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "desk"));
        cx.set_global(ClientAttachment::new(client, true, |_, _, _| {}));
    });
    struct HostsProbe(Entity<RemotePanel>);
    impl Render for HostsProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(
                self.0
                    .update(cx, |panel, cx| panel.render_hosts(window, cx)),
            )
        }
    }
    let state = cx.new(|_| WindowState::default());
    let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
        HostsProbe(cx.new(|cx| RemotePanel::new(state.clone(), window, cx)))
    });
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    // One frame starts whatever the change moves, the next draws it
    // settled.
    let draw = |cx: &mut gpui::VisualTestContext| {
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
            cx.executor().advance_clock(crate::edge_back::SETTLE);
        }
    };
    draw(cx);
    assert!(
        cx.debug_bounds("hosts-remote-settings").is_some(),
        "with hosting off, the page leads to the setting"
    );
    assert!(cx.debug_bounds("remote-invitation").is_none());
    assert!(cx.debug_bounds("hosting-settings").is_none());

    cx.update(|_, cx| {
        cx.update_global::<RemoteController, _>(|controller, _| {
            controller
                .start_hosting(
                    &TraverseSetting {
                        sources: Vec::new(),
                    },
                    "Studio".into(),
                )
                .unwrap()
        });
    });
    draw(cx);
    assert!(
        cx.debug_bounds("remote-invitation").is_some(),
        "hosting and accepting devices, the invitation is on the page"
    );
    assert!(cx.debug_bounds("hosts-remote-settings").is_none());
    assert!(
        cx.debug_bounds("hosting-settings").is_none(),
        "the hosting controls stay in Settings"
    );

    cx.update(|_, cx| {
        cx.update_global::<RemoteController, _>(|controller, _| {
            controller
                .hosting(tcode_protocol::HostingAction::SetEnabled(false))
                .unwrap();
        });
    });
    draw(cx);
    assert!(cx.debug_bounds("remote-invitation").is_none());
    assert!(
        cx.debug_bounds("hosts-remote-settings").is_some(),
        "with pairing off, the page leads to the setting"
    );
    assert!(cx.debug_bounds("hosting-settings").is_none());

    cx.update(|_, cx| {
        cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
    });
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(feature = "remote-hosting")]
#[gpui::test]
fn superseded_pairing_does_not_overwrite_the_saved_machine(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = crate::store::tests::scratch_root("stale-pairing");
    let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
    cx.update(|cx| cx.set_global(ClientAttachment::new(client.clone(), false, |_, _, _| {})));
    let (probe, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|_| WindowState::default());
        PairingProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
    });
    let host = |name: &str| PairedHost {
        host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
        name: name.into(),
        traverse: Vec::new(),
        relay: None,
        addrs: vec!["192.168.1.10:47420".into()],
        last_connected_unix: None,
        group_id: None,
        space_id: None,
        space_name: None,
    };
    probe.update_in(cx, |probe, window, cx| {
        probe.0.update(cx, |panel, cx| {
            let old = panel.form.restart();
            let current = panel.form.restart();
            panel.finish_pair(
                old,
                Err(tcode_client::pairing::PairError::Invalid),
                window,
                cx,
            );
            assert!(panel.form.error.is_none());
            assert!(client.load_hosts().unwrap().is_empty());
            panel.finish_pair(
                current,
                Err(tcode_client::pairing::PairError::Invalid),
                window,
                cx,
            );
            assert_eq!(
                panel.form.error.as_deref(),
                Some(crate::tr!("hosts.pair.rejected").as_ref())
            );
            assert!(client.load_hosts().unwrap().is_empty());
            let current = panel.form.restart();
            panel.finish_pair(current, Ok(host("current pairing")), window, cx);
            panel.finish_pair(old, Ok(host("superseded pairing")), window, cx);
        });
    });
    assert_eq!(client.load_hosts().unwrap(), vec![host("current pairing")]);
    let _ = std::fs::remove_dir_all(root);
}
