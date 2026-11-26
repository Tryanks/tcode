//! Machine groups: machines that admit the devices any member admits, so a
//! device paired with one member reaches all of them.
//!
//! Every member is equal; there is no coordinator and no group key. Each
//! machine keeps the shared [`GroupState`] in `traverse.json` beside its own
//! allow list, which stays the only admission check. Members exchange the
//! whole state over `tcode/group/1` and merge it entry by entry: the higher
//! `(counter, removed, by)` wins, so at an equal counter a removal beats an
//! admission, and a removed device returns only through a new pairing, which
//! writes the next counter.
//!
//! A machine's label, what the whole group calls it, is merged apart from
//! its entry: any member may set it for any machine, and the higher
//! `(counter, by)` of the label wins, so a state that carries no label (an
//! older build's) never takes one away.
//!
//! Inside a group there is one way in: a member's one-time invitation, which
//! then names the group. A device pairing with it joins the group, and so
//! does a machine presenting it over `tcode/group/1`. Every full-access
//! device of a member is a member; only space members stay on their machine.
use serde::{Deserialize, Serialize};
use tcode_client::pairing::{MAX_ADDRS, MAX_TRAVERSE, valid_host_id};

pub const ALPN_GROUP: &[u8] = b"tcode/group/1";
/// Bound on one `tcode/group/1` line, read before the dialer is known to be
/// a member.
pub const MAX_GROUP_LINE: usize = 512 * 1024;
/// Bound on a pairing or hello reply that carries the group's machines.
pub const MAX_ROSTER_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    Member,
    Removed,
}

/// The last change to one machine's or device's place in the group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub state: Membership,
    pub counter: u64,
    /// The machine that made the change.
    pub by: String,
    pub changed_unix: u64,
}

impl Entry {
    fn rank(&self) -> (u64, bool, &str) {
        (self.counter, self.state == Membership::Removed, &self.by)
    }

    pub fn is_member(&self) -> bool {
        self.state == Membership::Member
    }
}

/// A member machine and where to dial it. The routing hints are the
/// machine's own report, refreshed by the dialer from the connections it
/// makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMachine {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub entry: Entry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<MachineLabel>,
    /// What the machine takes from the others, as it reports itself with
    /// its name and hints; `None` from a build that does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts: Option<MachineAccepts>,
}

/// What the group calls one machine. A cleared label stays as `text: None`,
/// so the clearing outranks the label it cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineLabel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub counter: u64,
    /// The machine that set it.
    pub by: String,
}

impl MachineLabel {
    fn rank(&self) -> (u64, &str) {
        (self.counter, &self.by)
    }
}

/// Whether a machine takes the threads and the projects the other members
/// send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineAccepts {
    pub threads: bool,
    pub projects: bool,
}

impl GroupMachine {
    /// A machine with these hints; its entry is written by
    /// [`GroupState::set_machine`].
    pub fn new(
        id: String,
        name: String,
        traverse: Vec<String>,
        relay: Option<String>,
        addrs: Vec<String>,
    ) -> Self {
        Self {
            id,
            name,
            traverse,
            relay,
            addrs,
            label: None,
            accepts: None,
            entry: Entry {
                state: Membership::Member,
                counter: 0,
                by: String::new(),
                changed_unix: 0,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDevice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub entry: Entry,
}

/// What members exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupState {
    pub id: String,
    pub name: String,
    pub machines: Vec<GroupMachine>,
    pub devices: Vec<GroupDevice>,
}

/// A device admission this machine declined while **Accept new devices**
/// was off. It stays declined until the user admits it here or a newer
/// pairing elsewhere writes a later counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    pub id: String,
    pub counter: u64,
}

/// The group as `traverse.json` holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    #[serde(flatten)]
    pub state: GroupState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declined: Vec<Declined>,
}

/// One machine of the group as devices store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterMachine {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub member: bool,
    /// What the group calls the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The group's machines, sent to a full device with `paired` and `hello_ok`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub id: String,
    pub name: String,
    pub machines: Vec<RosterMachine>,
}

/// How a joining machine describes itself and the devices it brings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoiningMachine {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traverse: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<JoiningDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoiningDevice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// The lines of `tcode/group/1`: the dialer sends one, the accepter answers
/// with one, and the connection ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupLine {
    /// Present the secret of a member's invitation to join as a machine.
    Join {
        group: String,
        secret: String,
        machine: JoiningMachine,
    },
    /// The dialer's state, answered with the accepter's state after merging.
    Sync {
        state: GroupState,
    },
    Joined {
        state: GroupState,
    },
    Rejected {
        reason: GroupRejection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRejection {
    /// Wrong, expired or already used invitation.
    Invalid,
    /// The machine is not accepting new devices.
    Disabled,
    /// The machine could not record the change; try again.
    Busy,
    /// The dialer is not a member, or the group is not this machine's.
    NotMember,
}

impl JoiningMachine {
    pub fn is_valid(&self) -> bool {
        valid_name(&self.name)
            && valid_hints(&self.traverse, self.relay.as_deref(), &self.addrs)
            && self.devices.iter().all(|device| {
                valid_host_id(&device.id)
                    && valid_name(&device.name)
                    && device
                        .platform
                        .as_deref()
                        .is_none_or(|platform| platform.len() <= 256)
            })
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
}

fn valid_hints(traverse: &[String], relay: Option<&str>, addrs: &[String]) -> bool {
    traverse.len() <= MAX_TRAVERSE
        && traverse.iter().all(|value| value.len() <= 512)
        && relay.is_none_or(|relay| relay.len() <= 512)
        && addrs.len() <= MAX_ADDRS
        && addrs
            .iter()
            .all(|addr| addr.parse::<std::net::SocketAddr>().is_ok())
}

impl GroupState {
    pub fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            machines: Vec::new(),
            devices: Vec::new(),
        }
    }

    pub fn machine(&self, id: &str) -> Option<&GroupMachine> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    pub fn device(&self, id: &str) -> Option<&GroupDevice> {
        self.devices.iter().find(|device| device.id == id)
    }

    pub fn is_member_machine(&self, id: &str) -> bool {
        self.machine(id)
            .is_some_and(|machine| machine.entry.is_member())
    }

    pub fn member_machines(&self) -> impl Iterator<Item = &GroupMachine> {
        self.machines
            .iter()
            .filter(|machine| machine.entry.is_member())
    }

    fn next_entry(previous: Option<&Entry>, state: Membership, by: &str) -> Entry {
        Entry {
            state,
            counter: previous.map_or(1, |entry| entry.counter + 1),
            by: by.to_owned(),
            changed_unix: crate::identity::now_unix(),
        }
    }

    /// Record a change to a device's place in the group, made by `by`.
    pub fn set_device(
        &mut self,
        id: &str,
        name: &str,
        platform: Option<&str>,
        state: Membership,
        by: &str,
    ) {
        match self.devices.iter_mut().find(|device| device.id == id) {
            Some(device) => {
                device.entry = Self::next_entry(Some(&device.entry), state, by);
                device.name = name.to_owned();
                device.platform = platform.map(str::to_owned);
            }
            None => self.devices.push(GroupDevice {
                id: id.to_owned(),
                name: name.to_owned(),
                platform: platform.map(str::to_owned),
                entry: Self::next_entry(None, state, by),
            }),
        }
    }

    /// Record a change to a machine's place in the group, made by `by`,
    /// with the routing hints it has now. Its label stays as it is.
    pub fn set_machine(&mut self, machine: GroupMachine, state: Membership, by: &str) {
        match self
            .machines
            .iter_mut()
            .find(|known| known.id == machine.id)
        {
            Some(known) => {
                let entry = Self::next_entry(Some(&known.entry), state, by);
                let label = known.label.take();
                *known = GroupMachine {
                    entry,
                    label,
                    ..machine
                };
            }
            None => self.machines.push(GroupMachine {
                entry: Self::next_entry(None, state, by),
                ..machine
            }),
        }
    }

    /// Label `id` for the whole group, as `by`; `None` clears it. Returns
    /// whether the group has that machine.
    pub fn set_label(&mut self, id: &str, text: Option<String>, by: &str) -> bool {
        let Some(machine) = self.machines.iter_mut().find(|machine| machine.id == id) else {
            return false;
        };
        machine.label = Some(MachineLabel {
            text,
            counter: machine.label.as_ref().map_or(1, |label| label.counter + 1),
            by: by.to_owned(),
        });
        true
    }

    /// Take every entry and label of `other` that outranks this state's.
    /// Returns whether anything changed.
    pub fn merge(&mut self, other: GroupState) -> bool {
        let mut changed = false;
        for mut machine in other.machines {
            match self
                .machines
                .iter()
                .position(|known| known.id == machine.id)
            {
                Some(index) => {
                    let known = &mut self.machines[index];
                    let label = machine.label.take();
                    if machine.entry.rank() > known.entry.rank() {
                        machine.label = known.label.take();
                        *known = machine;
                        changed = true;
                    }
                    if let Some(label) = label
                        && known
                            .label
                            .as_ref()
                            .is_none_or(|ours| label.rank() > ours.rank())
                    {
                        known.label = Some(label);
                        changed = true;
                    }
                }
                None => {
                    self.machines.push(machine);
                    changed = true;
                }
            }
        }
        for device in other.devices {
            match self.devices.iter().position(|known| known.id == device.id) {
                Some(index) => {
                    if device.entry.rank() > self.devices[index].entry.rank() {
                        self.devices[index] = device;
                        changed = true;
                    }
                }
                None => {
                    self.devices.push(device);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Put where a connection just reached `id` ahead of its hints. These
    /// are this machine's own knowledge and change no entry. Returns whether
    /// anything changed.
    pub fn learn_hints(&mut self, id: &str, relay: Option<String>, addrs: Vec<String>) -> bool {
        let Some(machine) = self.machines.iter_mut().find(|machine| machine.id == id) else {
            return false;
        };
        let before = (machine.relay.clone(), machine.addrs.clone());
        if relay.is_some() {
            machine.relay = relay;
        }
        for addr in addrs.into_iter().rev() {
            machine.addrs.retain(|known| *known != addr);
            machine.addrs.insert(0, addr);
        }
        machine.addrs.truncate(MAX_ADDRS);
        (machine.relay.clone(), machine.addrs.clone()) != before
    }

    /// The machines a device stores, this one included.
    pub fn roster(&self) -> Roster {
        Roster {
            id: self.id.clone(),
            name: self.name.clone(),
            machines: self
                .machines
                .iter()
                .map(|machine| RosterMachine {
                    id: machine.id.clone(),
                    name: machine.name.clone(),
                    traverse: machine.traverse.clone(),
                    relay: machine.relay.clone(),
                    addrs: machine.addrs.clone(),
                    member: machine.entry.is_member(),
                    label: machine.label.as_ref().and_then(|label| label.text.clone()),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PHONE: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn state() -> GroupState {
        GroupState::new("AAAAAAAAAAAAAAAAAAAAAA".into(), "Home".into())
    }

    /// Two machines change the same device without hearing of each other:
    /// one admits it again, the other removes it. Whichever order the states
    /// meet in, both end with the removal, and a later pairing still wins.
    #[test]
    fn concurrent_changes_converge_on_the_removal_and_a_later_pairing_wins() {
        let mut origin = state();
        origin.set_device(PHONE, "Phone", None, Membership::Member, A);
        let mut on_a = origin.clone();
        let mut on_b = origin.clone();
        on_a.set_device(PHONE, "Phone", None, Membership::Member, A);
        on_b.set_device(PHONE, "Phone", None, Membership::Removed, B);

        let mut a_then_b = on_a.clone();
        assert!(a_then_b.merge(on_b.clone()));
        let mut b_then_a = on_b.clone();
        assert!(
            !b_then_a.merge(on_a.clone()),
            "the removal already outranks"
        );
        assert_eq!(a_then_b, b_then_a);
        assert!(!a_then_b.device(PHONE).unwrap().entry.is_member());

        a_then_b.set_device(PHONE, "Phone", None, Membership::Member, A);
        assert!(b_then_a.merge(a_then_b.clone()));
        assert!(b_then_a.device(PHONE).unwrap().entry.is_member());
    }

    fn with_machines() -> GroupState {
        let mut state = state();
        for id in [A, B] {
            state.set_machine(
                GroupMachine::new(id.into(), "Machine".into(), Vec::new(), None, Vec::new()),
                Membership::Member,
                id,
            );
        }
        state
    }

    /// Two members label the same machine without hearing of each other:
    /// whichever order the states meet in, both keep the same label, and a
    /// clearing written after it outranks it everywhere.
    #[test]
    fn concurrent_labels_converge_and_a_later_clearing_wins() {
        let origin = with_machines();
        let mut on_a = origin.clone();
        let mut on_b = origin.clone();
        assert!(on_a.set_label(A, Some("NAS".into()), A));
        assert!(on_b.set_label(A, Some("Attic".into()), B));

        let mut a_then_b = on_a.clone();
        a_then_b.merge(on_b.clone());
        let mut b_then_a = on_b.clone();
        b_then_a.merge(on_a.clone());
        assert_eq!(a_then_b, b_then_a);

        a_then_b.set_label(A, None, A);
        assert!(b_then_a.merge(a_then_b.clone()));
        assert_eq!(a_then_b, b_then_a);
        assert_eq!(
            b_then_a.machine(A).unwrap().label.as_ref().unwrap().text,
            None
        );
    }

    /// An older build's state names no labels and may carry a newer entry
    /// for the machine (it moved, or was renamed). The entry is taken and
    /// the label stays, as it does when the machine refreshes its own entry.
    #[test]
    fn a_label_outlives_a_state_and_an_entry_that_lack_it() {
        let mut ours = with_machines();
        ours.set_label(B, Some("Laptop".into()), A);
        let older: GroupState = serde_json::from_value(serde_json::json!({
            "id": "AAAAAAAAAAAAAAAAAAAAAA",
            "name": "Home",
            "machines": [{
                "id": B,
                "name": "Renamed",
                "relay": "https://relay.example/",
                "entry": {"state": "member", "counter": 7, "by": B, "changed_unix": 1}
            }],
            "devices": []
        }))
        .unwrap();
        assert_eq!(older.machine(B).unwrap().accepts, None);

        assert!(ours.merge(older));
        let machine = ours.machine(B).unwrap();
        assert_eq!(
            (machine.name.as_str(), machine.entry.counter),
            ("Renamed", 7)
        );
        assert_eq!(
            machine.label.as_ref().unwrap().text.as_deref(),
            Some("Laptop")
        );

        ours.set_machine(
            GroupMachine::new(B.into(), "Laptop B".into(), Vec::new(), None, Vec::new()),
            Membership::Member,
            B,
        );
        let roster = ours.roster();
        let listed = roster.machines.iter().find(|machine| machine.id == B);
        assert_eq!(listed.unwrap().label.as_deref(), Some("Laptop"));
    }
}
