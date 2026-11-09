//! `zwp_text_input_v3`, with the host as the input method: text the host's
//! input method composes or commits goes to the text field the client
//! enabled, and that field's caret goes back to the host to place its
//! candidates.
//!
//! smithay's `TextInputManagerState` relays to a `zwp_input_method_v2`
//! client and drops every request while none is bound, so the nest serves
//! the protocol itself.

use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor;

use super::State;
use crate::{Caret, Event, ViewId};

#[derive(Default)]
pub(super) struct TextInputs {
    instances: Vec<Instance>,
    /// The surface with keyboard focus, which text inputs of its client
    /// have entered.
    focus: Option<WlSurface>,
    /// What the host was last told, so it hears only changes.
    reported: Option<(ViewId, Option<Caret>)>,
}

struct Instance {
    resource: ZwpTextInputV3,
    /// Commits so far; `done` echoes it.
    serial: u32,
    enabled: bool,
    /// The caret, relative to the focused surface.
    cursor: Option<Rectangle<i32, Logical>>,
    pending_enabled: Option<bool>,
    pending_cursor: Option<Rectangle<i32, Logical>>,
}

impl TextInputs {
    fn of_client<'a>(&'a self, client: &'a ClientId) -> impl Iterator<Item = &'a Instance> {
        self.instances
            .iter()
            .filter(move |instance| belongs_to(instance, client))
    }

    fn of_client_mut<'a>(
        &'a mut self,
        client: &'a ClientId,
    ) -> impl Iterator<Item = &'a mut Instance> {
        self.instances
            .iter_mut()
            .filter(move |instance| belongs_to(instance, client))
    }
}

fn belongs_to(instance: &Instance, client: &ClientId) -> bool {
    instance
        .resource
        .client()
        .is_some_and(|owner| owner.id() == *client)
}

fn client_of(surface: &WlSurface) -> Option<ClientId> {
    surface.client().map(|client| client.id())
}

impl State {
    /// Moves text input to the surface that now has keyboard focus.
    pub(super) fn focus_text_input(&mut self, focused: Option<&WlSurface>) {
        if self.text_inputs.focus.as_ref() == focused {
            return;
        }
        if let Some(old) = self.text_inputs.focus.take()
            && let Some(client) = client_of(&old)
        {
            for instance in self.text_inputs.of_client_mut(&client) {
                instance.resource.leave(&old);
                instance.enabled = false;
                instance.cursor = None;
            }
        }
        if let Some(new) = focused
            && let Some(client) = client_of(new)
        {
            for instance in self.text_inputs.of_client(&client) {
                instance.resource.enter(new);
            }
        }
        self.text_inputs.focus = focused.cloned();
        self.report_text_input();
    }

    /// Inserts `text` in the enabled text field, ending any composition.
    pub(super) fn commit_text(&mut self, text: String) {
        self.with_enabled(|instance| {
            instance.resource.commit_string(Some(text.clone()));
            instance.resource.done(instance.serial);
        });
    }

    /// Shows `text` as the enabled text field's composition; empty text
    /// ends it.
    pub(super) fn compose_text(&mut self, text: String, cursor: Option<(usize, usize)>) {
        let (begin, end) = cursor
            .and_then(|(begin, end)| Some((i32::try_from(begin).ok()?, i32::try_from(end).ok()?)))
            .unwrap_or((-1, -1));
        let text = (!text.is_empty()).then_some(text);
        self.with_enabled(|instance| {
            instance.resource.preedit_string(text.clone(), begin, end);
            instance.resource.done(instance.serial);
        });
    }

    fn with_enabled(&mut self, mut f: impl FnMut(&Instance)) {
        let Some(client) = self.text_inputs.focus.as_ref().and_then(client_of) else {
            return;
        };
        let mut sent = false;
        for instance in self.text_inputs.of_client(&client) {
            if instance.enabled {
                f(instance);
                sent = true;
            }
        }
        if !sent {
            log::debug!("wayland nest: text input with no text field enabled");
        }
    }

    /// Tells the host where the focused view's text field draws its caret.
    fn report_text_input(&mut self) {
        let focus = self.text_inputs.focus.clone();
        let view = focus.as_ref().and_then(|surface| {
            let mut root = surface.clone();
            while let Some(parent) = compositor::get_parent(&root) {
                root = parent;
            }
            self.view_of(&root)
        });
        let caret = focus.as_ref().zip(view).and_then(|(surface, id)| {
            let client = client_of(surface)?;
            let cursor = self
                .text_inputs
                .of_client(&client)
                .find(|instance| instance.enabled)?
                .cursor?;
            let origin = self.surface_origin(self.views.get(&id)?, surface);
            Some(Caret {
                x: origin.x.saturating_add(cursor.loc.x),
                y: origin.y.saturating_add(cursor.loc.y),
                width: cursor.size.w,
                height: cursor.size.h,
            })
        });
        let report = view.map(|view| (view, caret));
        let previous = self.text_inputs.reported;
        if previous == report {
            return;
        }
        if let Some((old, Some(_))) = previous
            && report.is_none_or(|(view, _)| view != old)
        {
            let _ = self.events.try_send(Event::TextInput {
                view: old,
                caret: None,
            });
        }
        if let Some((view, caret)) = report {
            let _ = self.events.try_send(Event::TextInput { view, caret });
        }
        self.text_inputs.reported = report;
    }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTextInputManagerV3,
        request: zwp_text_input_manager_v3::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_text_input_manager_v3::Request::GetTextInput { id, .. } = request {
            let resource = data_init.init(id, ());
            if let Some(focus) = &state.text_inputs.focus
                && client_of(focus) == resource.client().map(|client| client.id())
            {
                resource.enter(focus);
            }
            state.text_inputs.instances.push(Instance {
                resource,
                serial: 0,
                enabled: false,
                cursor: None,
                pending_enabled: None,
                pending_cursor: None,
            });
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Some(instance) = state
            .text_inputs
            .instances
            .iter_mut()
            .find(|instance| instance.resource == *resource)
        else {
            return;
        };
        match request {
            zwp_text_input_v3::Request::Enable => instance.pending_enabled = Some(true),
            zwp_text_input_v3::Request::Disable => instance.pending_enabled = Some(false),
            zwp_text_input_v3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                instance.pending_cursor =
                    Some(Rectangle::new((x, y).into(), (width, height).into()));
            }
            zwp_text_input_v3::Request::Commit => {
                instance.serial = instance.serial.wrapping_add(1);
                match instance.pending_enabled.take() {
                    // Enabling starts from a fresh state.
                    Some(true) => {
                        instance.enabled = true;
                        instance.cursor = None;
                    }
                    Some(false) => instance.enabled = false,
                    None => {}
                }
                if let Some(cursor) = instance.pending_cursor.take() {
                    instance.cursor = Some(cursor);
                }
                state.report_text_input();
            }
            // Surrounding text, its change cause and the content type are
            // for an input method that edits around the caret; the host's
            // input method only composes and commits.
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, resource: &ZwpTextInputV3, _: &()) {
        state
            .text_inputs
            .instances
            .retain(|instance| instance.resource != *resource);
        state.report_text_input();
    }
}
