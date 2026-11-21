//! Which hosts this device's window shows at once, and in what order.
//!
//! A window shows one scope at a time: this machine alone, one saved machine
//! (standalone, a group member viewed on its own, or a space), or one machine
//! group side by side. Standalone machines and different groups never mix into
//! one multi-host view.

use tcode_client::host::{WorkspaceHost, WorkspacePreference, WorkspaceScope};
use tcode_client::pairing::PairedHost;

use crate::attachment::AttachmentTarget;
use crate::store::HostKey;

/// What a device's workspace is drawn from.
pub(crate) struct Device<'a> {
    pub saved: &'a [PairedHost],
    pub preference: Option<&'a WorkspacePreference>,
    /// The host the window was last on.
    pub last_host_id: Option<&'a str>,
    /// Whether this client has a host in its own process.
    pub local: bool,
    /// That host's machine id, which a saved record never stands in for.
    pub local_machine_id: Option<&'a str>,
    /// The group id that this client's own host belongs to, if any.
    pub local_group_id: Option<&'a str>,
    /// A browser reaches the one machine that served it.
    pub fixed_machine: bool,
}

fn joins_group(host: &PairedHost, group_id: &str, local_machine_id: Option<&str>) -> bool {
    host.space_id.is_none()
        && Some(host.host_id.as_str()) != local_machine_id
        && host.group_id.as_deref() == Some(group_id)
}

/// The display name of `group_id`, from the local machine's group when it
/// belongs to it or from any saved member of that group.
pub(crate) fn group_name<'a>(
    group_id: &str,
    saved: &'a [PairedHost],
    local_group: Option<&'a (String, String)>,
) -> Option<&'a str> {
    if let Some((id, name)) = local_group
        && id == group_id
        && !name.is_empty()
    {
        return Some(name.as_str());
    }
    saved.iter().find_map(|host| {
        (host.space_id.is_none() && host.group_id.as_deref() == Some(group_id))
            .then_some(host.group_name.as_deref())
            .flatten()
            .filter(|name| !name.is_empty())
    })
}

/// The scope this device's window is on. A preference with an explicit scope
/// wins; an older preference (no `scope`) or a fresh device resolves once from
/// its saved hosts, `last_host_id`, and this machine's group.
pub(crate) fn active_scope(device: &Device) -> WorkspaceScope {
    if device.fixed_machine {
        return device
            .saved
            .iter()
            .find(|host| Some(host.host_id.as_str()) == device.last_host_id)
            .or(device.saved.first())
            .map(|host| WorkspaceScope::Host {
                host_id: host.host_id.clone(),
            })
            .unwrap_or(WorkspaceScope::Empty);
    }
    if let Some(scope) = device
        .preference
        .and_then(|preference| preference.scope.as_ref())
    {
        return scope.clone();
    }
    let scope_of_host = |host: &PairedHost| -> WorkspaceScope {
        if host.space_id.is_none()
            && let Some(group_id) = &host.group_id
        {
            WorkspaceScope::Group {
                group_id: group_id.clone(),
            }
        } else {
            WorkspaceScope::Host {
                host_id: host.host_id.clone(),
            }
        }
    };
    if let Some(preference) = device.preference {
        // Legacy v0.4.0 preference (before `scope` existed).
        if preference.hosts.is_empty() {
            if device.local {
                return match device.local_group_id {
                    Some(group_id) => WorkspaceScope::Group {
                        group_id: group_id.to_owned(),
                    },
                    None => WorkspaceScope::Local,
                };
            }
            return WorkspaceScope::Empty;
        }
        let local_name = HostKey::Local.name();
        let anchor_id = device
            .last_host_id
            .filter(|id| preference.hosts.iter().any(|host| host.host_id == *id))
            .or_else(|| {
                preference
                    .hosts
                    .iter()
                    .find(|host| {
                        (host.host_id == local_name && device.local)
                            || device
                                .saved
                                .iter()
                                .any(|saved| saved.host_id == host.host_id)
                    })
                    .map(|host| host.host_id.as_str())
            });
        if let Some(id) = anchor_id {
            if id == local_name && device.local {
                if let Some(group_id) = device.local_group_id
                    && preference.hosts.iter().any(|listed| {
                        device.saved.iter().any(|saved| {
                            saved.host_id == listed.host_id
                                && joins_group(saved, group_id, device.local_machine_id)
                        })
                    })
                {
                    return WorkspaceScope::Group {
                        group_id: group_id.to_owned(),
                    };
                }
                return WorkspaceScope::Local;
            }
            if let Some(host) = device.saved.iter().find(|saved| saved.host_id == id) {
                return scope_of_host(host);
            }
        }
        return if device.local {
            WorkspaceScope::Local
        } else {
            WorkspaceScope::Empty
        };
    }
    // Fresh launch (no workspace preference yet).
    if device.local {
        return match device.local_group_id {
            Some(group_id) => WorkspaceScope::Group {
                group_id: group_id.to_owned(),
            },
            None => WorkspaceScope::Local,
        };
    }
    device
        .last_host_id
        .and_then(|id| device.saved.iter().find(|host| host.host_id == id))
        .or(device.saved.first())
        .map(scope_of_host)
        .unwrap_or(WorkspaceScope::Empty)
}

/// The hosts shown by the device's active scope, in order.
pub(crate) fn members(device: &Device) -> Vec<AttachmentTarget> {
    let remote = |host: &PairedHost| AttachmentTarget::Remote(Box::new(host.clone()));
    match active_scope(device) {
        WorkspaceScope::Empty => Vec::new(),
        WorkspaceScope::Local => device
            .local
            .then_some(AttachmentTarget::Local)
            .into_iter()
            .collect(),
        WorkspaceScope::Host { host_id } => {
            if device.local && Some(host_id.as_str()) == device.local_machine_id {
                return vec![AttachmentTarget::Local];
            }
            device
                .saved
                .iter()
                .find(|host| host.host_id == host_id)
                .map(remote)
                .into_iter()
                .collect()
        }
        WorkspaceScope::Group { group_id } => {
            let local_in_group = device.local && device.local_group_id == Some(group_id.as_str());
            let left = device
                .preference
                .map_or(&[][..], |preference| &preference.left[..]);
            let listed: Vec<&str> = device
                .preference
                .map(|preference| {
                    preference
                        .hosts
                        .iter()
                        .map(|host| host.host_id.as_str())
                        .collect()
                })
                .unwrap_or_default();
            let mut members = Vec::new();
            let local_name = HostKey::Local.name();
            if local_in_group && !listed.contains(&local_name) {
                members.push(AttachmentTarget::Local);
            }
            for id in listed {
                let member = if id == local_name {
                    local_in_group.then_some(AttachmentTarget::Local)
                } else {
                    device
                        .saved
                        .iter()
                        .find(|host| {
                            host.host_id == id
                                && joins_group(host, &group_id, device.local_machine_id)
                                && !left.contains(&host.host_id)
                        })
                        .map(remote)
                };
                if let Some(member) = member
                    && !members.contains(&member)
                {
                    members.push(member);
                }
            }
            for host in device.saved {
                if joins_group(host, &group_id, device.local_machine_id)
                    && !left.contains(&host.host_id)
                {
                    let member = remote(host);
                    if !members.contains(&member) {
                        members.push(member);
                    }
                }
            }
            members
        }
    }
}

/// Set the active scope without discarding host order, fold states, or hidden
/// group members kept for other views.
pub(crate) fn set_scope(workspace: &mut WorkspacePreference, scope: WorkspaceScope) {
    workspace.scope = Some(scope);
}

/// Include `host_id` back in its group's multi-host view.
pub(crate) fn include_in_group(workspace: &mut WorkspacePreference, host_id: &str) {
    workspace.left.retain(|left| left != host_id);
}

/// Hide a group member from its group's multi-host view.
pub(crate) fn leave_group_member(workspace: &mut WorkspacePreference, host: &HostKey) {
    if !workspace.left.iter().any(|left| left == host.name()) {
        workspace.left.push(host.name().to_owned());
    }
}

/// Clear any hidden-member records for `host_ids` (used when entering a group
/// whose every member had been hidden).
pub(crate) fn clear_left_for<'a>(
    workspace: &mut WorkspacePreference,
    host_ids: impl IntoIterator<Item = &'a str>,
) {
    let ids: Vec<&str> = host_ids.into_iter().collect();
    workspace.left.retain(|left| !ids.contains(&left.as_str()));
}

/// Move `host` one place earlier, or later, among `members`, keeping order and
/// fold state recorded for hosts outside `members`.
pub(crate) fn shift(
    workspace: &mut WorkspacePreference,
    members: &[HostKey],
    host: &HostKey,
    earlier: bool,
) {
    let Some(at) = members.iter().position(|member| member == host) else {
        return;
    };
    let to = if earlier {
        at.checked_sub(1)
    } else {
        Some(at + 1).filter(|to| *to < members.len())
    };
    let Some(to) = to else {
        return;
    };
    let mut order = members.to_vec();
    order.swap(at, to);
    let mut kept: Vec<WorkspaceHost> = workspace
        .hosts
        .iter()
        .filter(|listed| !order.iter().any(|key| key.name() == listed.host_id))
        .cloned()
        .collect();
    for key in order {
        let collapsed = workspace
            .hosts
            .iter()
            .any(|listed| listed.host_id == key.name() && listed.collapsed);
        kept.push(WorkspaceHost {
            host_id: key.name().to_owned(),
            collapsed,
        });
    }
    workspace.hosts = kept;
}

/// Fold `host`'s section away or open it again.
pub(crate) fn set_collapsed(workspace: &mut WorkspacePreference, host: &HostKey, collapsed: bool) {
    match workspace
        .hosts
        .iter_mut()
        .find(|listed| listed.host_id == host.name())
    {
        Some(listed) => listed.collapsed = collapsed,
        None if collapsed => workspace.hosts.push(WorkspaceHost {
            host_id: host.name().to_owned(),
            collapsed,
        }),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, group: Option<&str>, space: bool) -> PairedHost {
        PairedHost {
            host_id: id.into(),
            name: id.into(),
            traverse: Vec::new(),
            relay: None,
            addrs: Vec::new(),
            last_connected_unix: None,
            group_id: group.map(str::to_owned),
            group_name: group.map(|g| format!("Group {g}")),
            space_id: space.then(|| "s".into()),
            space_name: space.then(|| "Shared".into()),
        }
    }

    fn pref(scope: Option<WorkspaceScope>, hosts: &[&str], left: &[&str]) -> WorkspacePreference {
        WorkspacePreference {
            scope,
            hosts: hosts
                .iter()
                .map(|id| WorkspaceHost {
                    host_id: (*id).into(),
                    collapsed: false,
                })
                .collect(),
            left: left.iter().map(|id| (*id).into()).collect(),
        }
    }

    /// Each scope shows only its own hosts: standalone hosts never mix with
    /// machine groups, different groups stay separate, and this machine joins
    /// only the group it belongs to.
    #[test]
    fn scope_isolates_standalone_hosts_spaces_and_machine_groups() {
        let solo = saved("solo", None, false);
        let other = saved("other", None, false);
        let g1_a = saved("g1-a", Some("g1"), false);
        let g1_b = saved("g1-b", Some("g1"), false);
        let g2_a = saved("g2-a", Some("g2"), false);
        let space = saved("space", None, true);
        let own = saved("own", Some("g1"), false);
        let all = [
            solo.clone(),
            other.clone(),
            g1_a.clone(),
            g1_b.clone(),
            g2_a.clone(),
            space.clone(),
            own.clone(),
        ];
        let g1_ordered = pref(
            Some(WorkspaceScope::Group {
                group_id: "g1".into(),
            }),
            &["g1-b", "local", "g1-a"],
            &[],
        );
        let g1_left = pref(
            Some(WorkspaceScope::Group {
                group_id: "g1".into(),
            }),
            &[],
            &["g1-b"],
        );
        let solo_scope = pref(
            Some(WorkspaceScope::Host {
                host_id: "solo".into(),
            }),
            &["g1-b", "g1-a"],
            &[],
        );
        let member_alone = pref(
            Some(WorkspaceScope::Host {
                host_id: "g1-a".into(),
            }),
            &[],
            &[],
        );
        let local_alone = pref(Some(WorkspaceScope::Local), &[], &[]);
        let g2_scope = pref(
            Some(WorkspaceScope::Group {
                group_id: "g2".into(),
            }),
            &[],
            &[],
        );
        let legacy_solo = pref(None, &["solo", "g1-a", "g1-b"], &[]);
        let legacy_group = pref(None, &["g1-a", "g1-b", "solo"], &[]);
        let legacy_empty = pref(None, &[], &[]);

        let cases: [(&str, Device, &[&str]); 14] = [
            (
                "fresh desktop not in a group opens local alone",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: Some("solo"),
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["local"],
            ),
            (
                "fresh desktop in g1 opens g1 with local, excluding solo and g2",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: Some("g1"),
                    fixed_machine: false,
                },
                &["local", "g1-a", "g1-b"],
            ),
            (
                "desktop explicitly on local stays on local alone even when in a group",
                Device {
                    saved: &all,
                    preference: Some(&local_alone),
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: Some("g1"),
                    fixed_machine: false,
                },
                &["local"],
            ),
            (
                "desktop viewing remote group g2 does not pull in local from g1",
                Device {
                    saved: &all,
                    preference: Some(&g2_scope),
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: Some("g1"),
                    fixed_machine: false,
                },
                &["g2-a"],
            ),
            (
                "phone viewing standalone host excludes every group member",
                Device {
                    saved: &all,
                    preference: Some(&solo_scope),
                    last_host_id: None,
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["solo"],
            ),
            (
                "phone viewing one member of a group alone shows only that member",
                Device {
                    saved: &all,
                    preference: Some(&member_alone),
                    last_host_id: None,
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["g1-a"],
            ),
            (
                "group view respects saved order including local",
                Device {
                    saved: &all,
                    preference: Some(&g1_ordered),
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: Some("g1"),
                    fixed_machine: false,
                },
                &["g1-b", "local", "g1-a"],
            ),
            (
                "hidden group members stay out of the group view",
                Device {
                    saved: &all,
                    preference: Some(&g1_left),
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    local_group_id: Some("g1"),
                    fixed_machine: false,
                },
                &["local", "g1-a"],
            ),
            (
                "fresh phone last on standalone host opens only that host",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: Some("solo"),
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["solo"],
            ),
            (
                "fresh phone last on a space opens only that space",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: Some("space"),
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["space"],
            ),
            (
                "legacy mobile preference anchored on solo migrates to solo only",
                Device {
                    saved: &all,
                    preference: Some(&legacy_solo),
                    last_host_id: Some("solo"),
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["solo"],
            ),
            (
                "legacy mobile preference anchored on group member migrates to that group only",
                Device {
                    saved: &all,
                    preference: Some(&legacy_group),
                    last_host_id: Some("g1-a"),
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &["g1-a", "g1-b", "own"],
            ),
            (
                "legacy empty mobile preference stays empty",
                Device {
                    saved: &all,
                    preference: Some(&legacy_empty),
                    last_host_id: None,
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: false,
                },
                &[],
            ),
            (
                "browser has only its serving machine",
                Device {
                    saved: &[other.clone(), solo.clone()],
                    preference: Some(&g1_ordered),
                    last_host_id: Some("solo"),
                    local: false,
                    local_machine_id: None,
                    local_group_id: None,
                    fixed_machine: true,
                },
                &["solo"],
            ),
        ];

        for (case, device, expected) in cases {
            let members: Vec<String> = members(&device)
                .iter()
                .map(|member| member.key().name().to_owned())
                .collect();
            assert_eq!(members, expected, "{case}");
        }
    }
}
