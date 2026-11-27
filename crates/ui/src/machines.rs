//! The other machines of a group as this window names and reaches them. A
//! thread one of them runs is that machine's: what the window shows of it
//! comes from that machine's store while the window is linked to it, and
//! otherwise only from what the host that asked for it kept, its reference
//! and last known state.

use gpui::{App, Context, Entity, SharedString, Subscription};
use tcode_core::control::{RemoteChild, RemoteChildOp, RemoteChildState};
use tcode_core::settlement::{AgentExecution, AgentReport};

use crate::store::{HostKey, TopicKind, WorkspaceStore};

/// What the window knows of the group's machines without asking a host:
/// this process's own machine and the machines this device saved, read from
/// disk when the window opens and when the saved machines change, and the
/// stores of the hosts the window is linked to. An entity of its own, apart
/// from the shell, so that what the shell draws can name a machine.
#[derive(Default)]
pub(crate) struct Directory {
    local: Option<String>,
    saved: Vec<tcode_client::pairing::PairedHost>,
    stores: Vec<(HostKey, Entity<WorkspaceStore>)>,
}

impl Directory {
    pub(crate) fn read(client: &dyn tcode_client::host::ClientHost) -> Self {
        Self {
            local: client.local_machine_id().ok().flatten(),
            saved: client.load_hosts().unwrap_or_default(),
            stores: Vec::new(),
        }
    }

    /// Read this process's machine and the saved machines again.
    pub(crate) fn reread(&mut self, client: &dyn tcode_client::host::ClientHost) {
        let stores = std::mem::take(&mut self.stores);
        *self = Self {
            stores,
            ..Self::read(client)
        };
    }

    /// Follow the window onto the hosts it is linked to now.
    pub(crate) fn set_stores(&mut self, stores: Vec<(HostKey, Entity<WorkspaceStore>)>) {
        self.stores = stores;
    }

    /// The store of machine `id`, while the window is linked to it.
    fn store_of(&self, id: &str) -> Option<Entity<WorkspaceStore>> {
        let host = if self.local.as_deref() == Some(id) {
            HostKey::Local
        } else {
            HostKey::Remote(id.to_owned())
        };
        self.stores
            .iter()
            .find(|(key, _)| *key == host)
            .map(|(_, store)| store.clone())
    }

    fn saved(&self, id: &str) -> Option<&tcode_client::pairing::PairedHost> {
        self.saved.iter().find(|saved| saved.host_id == id)
    }
}

/// A machine of the group, as this window has it.
pub(crate) struct Machine {
    pub(crate) id: String,
    /// Its group's label for it, else its name, else the head of its id.
    pub(crate) name: String,
    /// Its workspace, while this window is linked to it.
    pub(crate) store: Option<Entity<WorkspaceStore>>,
    /// Whether that link is up: neither reconnecting nor offline. Up, and
    /// still loading what the machine holds, counts.
    pub(crate) connected: bool,
    /// When this device last reached it (unix seconds), as it recorded it.
    pub(crate) last_connected: Option<u64>,
}

/// The machines of the group `store`'s machine is in, as that machine last
/// reported them; empty outside a group or where this client may not manage
/// the machine.
pub(crate) fn group_machines(
    store: &Entity<WorkspaceStore>,
    cx: &mut App,
) -> Vec<tcode_protocol::GroupMachineInfo> {
    crate::remote::spaces::for_store(store, cx)
        .and_then(|spaces| spaces.read(cx).group().map(|group| group.machines.clone()))
        .unwrap_or_default()
}

/// What `store`'s own machine is called: its group's label for it, else its
/// own name; `None` while it has neither.
pub(crate) fn own_name(store: &Entity<WorkspaceStore>, cx: &mut App) -> Option<String> {
    group_machines(store, cx)
        .into_iter()
        .find(|machine| machine.this_machine)
        .map(|machine| machine.title().to_owned())
        .or_else(|| named_by_store(store.read(cx), cx))
        .filter(|name| !name.trim().is_empty())
}

/// What `store`'s machine is called by the store itself: this process's own
/// name for the local one, the name the window attached it under otherwise.
fn named_by_store(store: &WorkspaceStore, cx: &App) -> Option<String> {
    match store.host_key() {
        HostKey::Local => crate::remote::local_machine_name(cx),
        HostKey::Remote(_) => store.remote_host_name().map(str::to_owned),
    }
}

/// [`WorkspaceStore::host_title`], with a machine of a group called by its
/// label: a device reaching only a space has no group to ask.
pub(crate) fn host_title(store: &Entity<WorkspaceStore>, cx: &mut App) -> SharedString {
    let (remote, full) = {
        let store = store.read(cx);
        (store.is_remote(), store.scope().is_full())
    };
    if remote
        && full
        && let Some(name) = own_name(store, cx)
    {
        return name.into();
    }
    store.read(cx).host_title()
}

/// Machine `id`, named by the open store for it, else by the machine this
/// device saved, else by the head of its id.
pub(crate) fn machine(id: &str, cx: &App) -> Machine {
    resolve(id, None, cx)
}

/// Machine `id` as [`machine`] names it, but called first what the group of
/// any of `hosts`' machines calls it.
pub(crate) fn machine_in_groups(
    hosts: &[Entity<WorkspaceStore>],
    id: &str,
    cx: &mut App,
) -> Machine {
    let mut title = None;
    for host in hosts {
        title = group_machines(host, cx)
            .into_iter()
            .find(|machine| machine.id == id)
            .map(|machine| machine.title().to_owned());
        if title.is_some() {
            break;
        }
    }
    resolve(id, title, cx)
}

fn resolve(id: &str, group_title: Option<String>, cx: &App) -> Machine {
    let directory = crate::shell::machine_directory(cx);
    let directory = directory.as_ref().map(|directory| directory.read(cx));
    let store = directory.and_then(|directory| directory.store_of(id));
    let saved = directory.and_then(|directory| directory.saved(id)).cloned();
    let open_name = store
        .as_ref()
        .and_then(|store| named_by_store(store.read(cx), cx));
    let name = group_title
        .or(open_name)
        .or_else(|| saved.as_ref().map(|saved| saved.title().to_owned()))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| id.chars().take(8).collect());
    Machine {
        id: id.to_owned(),
        name,
        connected: store.as_ref().is_some_and(|store| {
            !matches!(
                store.read(cx).connection_state(),
                tcode_client::ConnectionState::Reconnecting { .. }
                    | tcode_client::ConnectionState::Offline { .. }
            )
        }),
        store,
        last_connected: saved.and_then(|saved| saved.last_connected_unix),
    }
}

/// What a thread elsewhere is called in a message: `<thread>@<machine>`,
/// split into the thread and, when it is another machine's, that machine.
pub(crate) fn elsewhere(id: &str) -> (&str, Option<&str>) {
    match id.split_once('@') {
        Some((thread, machine)) if !machine.is_empty() => (thread, Some(machine)),
        _ => (id, None),
    }
}

/// The locale's words for "`actor` on `machine`", split around where the
/// actor goes, so the actor can be drawn as a link of its own.
pub(crate) fn on_machine_parts(machine: &str) -> (String, String) {
    const ACTOR: &str = "\u{2}";
    let template = crate::tr!(
        "machines.dispatch.on_machine",
        actor = ACTOR,
        machine = machine
    )
    .into_owned();
    match template.split_once(ACTOR) {
        Some((before, after)) => (before.to_owned(), after.to_owned()),
        None => (String::new(), template),
    }
}

/// `actor`, a thread's title, on the machine named `machine` when it is
/// another machine's.
pub(crate) fn actor_label(actor: &str, machine: Option<&str>, cx: &App) -> String {
    match machine {
        Some(machine) => {
            let name = self::machine(machine, cx).name;
            crate::tr!(
                "machines.dispatch.on_machine",
                actor = actor,
                machine = name.as_str()
            )
            .into_owned()
        }
        None => actor.to_owned(),
    }
}

/// Show thread `thread` (at `position`) where its machine's store places
/// threads. `false`, and nothing done, while this window is not linked to
/// that machine.
pub(crate) fn open_thread(
    machine: &str,
    thread: &str,
    position: Option<u64>,
    cx: &mut App,
) -> bool {
    let Some(store) = self::machine(machine, cx).store else {
        return false;
    };
    let thread = thread.to_owned();
    store.update(cx, |store, cx| match position {
        Some(position) => store.show_thread_at_position(thread, position, cx),
        None => store.show_thread(thread, cx),
    });
    true
}

/// Why something that opens a thread on `machine` does not: the window is
/// not linked to it.
pub(crate) fn not_open(machine: &str) -> SharedString {
    crate::tr!("machines.dispatch.not_open", machine = machine)
        .into_owned()
        .into()
}

/// How thread `id` runs, as its own machine's store reports it. A thread
/// started from another machine is a root thread there, which the host
/// reports no agent status for, so this reads the thread's own activity.
/// `None` where the store does not know the thread.
pub(crate) fn execution(store: &WorkspaceStore, id: &str) -> Option<AgentExecution> {
    let meta = store.thread_meta(id)?;
    let activity = store.activity(id);
    let has = |test: fn(&tcode_protocol::SessionActivity) -> bool| activity.is_some_and(test);
    Some(
        if has(|activity| activity.waiting_for_approval || activity.waiting_for_input) {
            AgentExecution::Waiting
        } else if has(|activity| activity.working) {
            AgentExecution::Working
        } else if has(|activity| activity.background.tasks > 0 || activity.background.agents > 0) {
            AgentExecution::Background
        } else if meta.cancelled_at.is_some() {
            AgentExecution::Cancelled
        } else if has(|activity| activity.failed) || meta.report() == AgentReport::Failed {
            AgentExecution::Failed
        } else {
            AgentExecution::Finished
        },
    )
}

/// How a thread a lead started on another machine stands, in a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    Execution(AgentExecution),
    /// Running as last heard, and its machine is out of this window's reach.
    Unreachable,
    /// Its machine left the group.
    MachineLeft,
}

/// How `child` stands: from its machine's store while the window is linked
/// to that machine and the store knows the thread, else from the lead's
/// record of it.
pub(crate) fn standing(child: &RemoteChild, machine: &Machine, cx: &App) -> Standing {
    if machine.connected
        && let Some(execution) = machine
            .store
            .as_ref()
            .and_then(|store| execution(store.read(cx), &child.thread_id))
    {
        return Standing::Execution(execution);
    }
    match child.state {
        RemoteChildState::Running if !machine.connected => Standing::Unreachable,
        RemoteChildState::Running => Standing::Execution(AgentExecution::Working),
        RemoteChildState::Reported | RemoteChildState::Settled => {
            Standing::Execution(AgentExecution::Finished)
        }
        RemoteChildState::Failed => Standing::Execution(AgentExecution::Failed),
        RemoteChildState::Cancelled => Standing::Execution(AgentExecution::Cancelled),
        RemoteChildState::MachineLeft => Standing::MachineLeft,
    }
}

/// The muted lines under a thread elsewhere: what its lead still has to
/// tell its machine, and why it may not be heard from.
pub(crate) fn notes(
    child: &RemoteChild,
    standing: Standing,
    machine: &Machine,
    cx: &App,
) -> Vec<String> {
    let name = machine.name.as_str();
    let mut notes = Vec::new();
    if let Some(op) = child.pending {
        let key = match op {
            RemoteChildOp::Cancel => "machines.dispatch.pending_cancel",
            RemoteChildOp::Interrupt => "machines.dispatch.pending_interrupt",
            RemoteChildOp::Settle => "machines.dispatch.pending_settle",
        };
        notes.push(crate::tr!(key, machine = name).into_owned());
    }
    if standing == Standing::Unreachable {
        notes.push(match machine.last_connected {
            Some(at) => {
                let ago = crate::time::humanize_ago(crate::time::now_secs(cx).saturating_sub(at));
                crate::tr!(
                    "machines.dispatch.unreachable_since",
                    machine = name,
                    ago = ago.as_str()
                )
                .into_owned()
            }
            None => crate::tr!("machines.dispatch.unreachable_now", machine = name).into_owned(),
        });
    }
    notes
}

/// Subscriptions to the stores of the machines a view shows threads of, so
/// it is drawn again when one of them changes or its link comes or goes.
#[derive(Default)]
pub(crate) struct Followed(Vec<(HostKey, Subscription)>);

impl Followed {
    /// Follow the stores of `machines` this window is linked to, and only
    /// those, calling `changed` on the view when one of them changes.
    pub(crate) fn follow<V: 'static>(
        &mut self,
        machines: impl IntoIterator<Item = String>,
        changed: fn(&mut V, &mut Context<V>),
        cx: &mut Context<V>,
    ) {
        let stores: Vec<Entity<WorkspaceStore>> = machines
            .into_iter()
            .filter_map(|machine| self::machine(&machine, cx).store)
            .collect();
        let keys: Vec<HostKey> = stores
            .iter()
            .map(|store| store.read(cx).host_key())
            .collect();
        self.0.retain(|(key, _)| keys.contains(key));
        for (store, key) in stores.into_iter().zip(keys) {
            if self.0.iter().any(|(followed, _)| *followed == key) {
                continue;
            }
            let subscription = cx.subscribe(
                &store,
                move |view, _, change: &crate::store::StoreChange, cx| {
                    if matches!(change.topic, TopicKind::Index | TopicKind::SessionStatus) {
                        changed(view, cx);
                    }
                },
            );
            self.0.push((key, subscription));
        }
    }
}
