//! Which hosts this device's window lists side by side, and in what order.

use tcode_client::host::{WorkspaceHost, WorkspacePreference};
use tcode_client::pairing::PairedHost;

use crate::remote::AttachmentTarget;
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
    /// A browser reaches the one machine that served it.
    pub fixed_machine: bool,
}

fn joins(host: &PairedHost) -> bool {
    host.space_id.is_none()
}

/// Whether `target` can be one of several: a space's host is reached alone,
/// in place of the others.
pub(crate) fn joins_target(target: &AttachmentTarget) -> bool {
    match target {
        AttachmentTarget::Local => true,
        AttachmentTarget::Remote(host) => joins(host),
    }
}

/// The workspace's hosts, in order. The local host comes first unless the
/// device placed it; then the hosts the device listed; then every member of
/// a group the device has not left. A device that has never arranged its
/// workspace has the local host, the one it was last on and its groups.
pub(crate) fn members(device: &Device) -> Vec<AttachmentTarget> {
    let remote = |host: &PairedHost| AttachmentTarget::Remote(Box::new(host.clone()));
    if device.fixed_machine {
        return device
            .saved
            .iter()
            .find(|host| Some(host.host_id.as_str()) == device.last_host_id)
            .or(device.saved.first())
            .map(remote)
            .into_iter()
            .collect();
    }
    let eligible =
        |host: &&PairedHost| joins(host) && Some(host.host_id.as_str()) != device.local_machine_id;
    let listed: Vec<&str> = match device.preference {
        Some(preference) => preference
            .hosts
            .iter()
            .map(|host| host.host_id.as_str())
            .collect(),
        None => device.last_host_id.into_iter().collect(),
    };
    let mut members = Vec::new();
    let local = HostKey::Local;
    if device.local && !listed.contains(&local.name()) {
        members.push(AttachmentTarget::Local);
    }
    for id in listed {
        let member = if id == local.name() {
            device.local.then_some(AttachmentTarget::Local)
        } else {
            device
                .saved
                .iter()
                .filter(eligible)
                .find(|host| host.host_id == id)
                .map(remote)
        };
        if let Some(member) = member
            && !members.contains(&member)
        {
            members.push(member);
        }
    }
    let left = device
        .preference
        .map_or(&[][..], |preference| &preference.left[..]);
    for host in device.saved.iter().filter(eligible) {
        let member = remote(host);
        if host.group_id.is_some() && !left.contains(&host.host_id) && !members.contains(&member) {
            members.push(member);
        }
    }
    members
}

/// The workspace `members` are, keeping what `preference` says beside them.
pub(crate) fn arranged(
    preference: Option<&WorkspacePreference>,
    members: &[HostKey],
) -> WorkspacePreference {
    let collapsed = |key: &HostKey| {
        preference.is_some_and(|preference| {
            preference
                .hosts
                .iter()
                .any(|host| host.host_id == key.name() && host.collapsed)
        })
    };
    WorkspacePreference {
        hosts: members
            .iter()
            .map(|key| WorkspaceHost {
                host_id: key.name().to_owned(),
                collapsed: collapsed(key),
            })
            .collect(),
        left: preference
            .map(|preference| preference.left.clone())
            .unwrap_or_default(),
    }
}

/// `host` is in the workspace, after the hosts already there.
pub(crate) fn join(workspace: &mut WorkspacePreference, host: &HostKey) {
    workspace.left.retain(|left| left != host.name());
    if !workspace
        .hosts
        .iter()
        .any(|listed| listed.host_id == host.name())
    {
        workspace.hosts.push(WorkspaceHost {
            host_id: host.name().to_owned(),
            collapsed: false,
        });
    }
}

/// `host` is out of the workspace, and stays out though a group it is in
/// would bring it back.
pub(crate) fn leave(workspace: &mut WorkspacePreference, host: &HostKey) {
    workspace
        .hosts
        .retain(|listed| listed.host_id != host.name());
    if !workspace.left.iter().any(|left| left == host.name()) {
        workspace.left.push(host.name().to_owned());
    }
}

/// Move `host` one place earlier, or later.
pub(crate) fn shift(workspace: &mut WorkspacePreference, host: &HostKey, earlier: bool) {
    let Some(at) = workspace
        .hosts
        .iter()
        .position(|listed| listed.host_id == host.name())
    else {
        return;
    };
    let to = if earlier {
        at.checked_sub(1)
    } else {
        Some(at + 1).filter(|to| *to < workspace.hosts.len())
    };
    if let Some(to) = to {
        workspace.hosts.swap(at, to);
    }
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

    fn saved(id: &str, group: bool, space: bool) -> PairedHost {
        PairedHost {
            host_id: id.into(),
            name: id.into(),
            traverse: Vec::new(),
            relay: None,
            addrs: Vec::new(),
            last_connected_unix: None,
            group_id: group.then(|| "g".into()),
            space_id: space.then(|| "s".into()),
            space_name: space.then(|| "Shared".into()),
        }
    }

    fn listed(hosts: &[&str], left: &[&str]) -> WorkspacePreference {
        WorkspacePreference {
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

    /// Who is in a device's workspace, in order, for each way a device can
    /// stand: upgraded with no workspace yet, arranged, with groups it left,
    /// with a space, a browser, and a saved record of its own machine.
    #[test]
    fn a_devices_workspace_is_its_local_host_its_listed_hosts_and_its_groups() {
        let solo = saved("solo", false, false);
        let other = saved("other", false, false);
        let member = saved("member", true, false);
        let second = saved("second", true, false);
        let space = saved("space", false, true);
        let own = saved("own", true, false);
        let all = [
            solo.clone(),
            other.clone(),
            member.clone(),
            second.clone(),
            space.clone(),
            own.clone(),
        ];
        let arranged = listed(&["second", "solo", "local"], &[]);
        let left = listed(&["solo"], &["member", "second"]);
        let cases: [(&str, Device, &[&str]); 9] = [
            (
                "an upgraded desktop: last host, then the groups",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: Some("solo"),
                    local: true,
                    local_machine_id: Some("own"),
                    fixed_machine: false,
                },
                &["local", "solo", "member", "second"],
            ),
            (
                "an upgraded phone last on a space joins only the groups",
                Device {
                    saved: &all,
                    preference: None,
                    last_host_id: Some("space"),
                    local: false,
                    local_machine_id: None,
                    fixed_machine: false,
                },
                &["member", "second", "own"],
            ),
            (
                "an arranged workspace keeps its order, local host included",
                Device {
                    saved: &all,
                    preference: Some(&arranged),
                    last_host_id: Some("other"),
                    local: true,
                    local_machine_id: Some("own"),
                    fixed_machine: false,
                },
                &["second", "solo", "local", "member"],
            ),
            (
                "groups left stay out",
                Device {
                    saved: &all,
                    preference: Some(&left),
                    last_host_id: None,
                    local: true,
                    local_machine_id: Some("own"),
                    fixed_machine: false,
                },
                &["local", "solo"],
            ),
            (
                "a listed host no longer saved is gone",
                Device {
                    saved: std::slice::from_ref(&member),
                    preference: Some(&arranged),
                    last_host_id: None,
                    local: false,
                    local_machine_id: None,
                    fixed_machine: false,
                },
                &["member"],
            ),
            (
                "a space never joins, though listed",
                Device {
                    saved: &all,
                    preference: Some(&listed(&["space"], &["member", "second", "own"])),
                    last_host_id: Some("space"),
                    local: false,
                    local_machine_id: None,
                    fixed_machine: false,
                },
                &[],
            ),
            (
                "a client with no host of its own lists none, though listed",
                Device {
                    saved: std::slice::from_ref(&solo),
                    preference: Some(&listed(&["local", "solo"], &[])),
                    last_host_id: None,
                    local: false,
                    local_machine_id: None,
                    fixed_machine: false,
                },
                &["solo"],
            ),
            (
                "a browser has its one machine",
                Device {
                    saved: &[other.clone(), solo.clone()],
                    preference: Some(&arranged),
                    last_host_id: Some("solo"),
                    local: false,
                    local_machine_id: None,
                    fixed_machine: true,
                },
                &["solo"],
            ),
            (
                "nothing saved and no host of its own",
                Device {
                    saved: &[],
                    preference: None,
                    last_host_id: Some("gone"),
                    local: false,
                    local_machine_id: None,
                    fixed_machine: false,
                },
                &[],
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
