use super::{
    ActionRequest, ActionResult, Backend, BackendError, ObserveRequest, RootFilters, RootInfo,
    RootObservation,
};

pub(super) struct StubBackend;

impl Backend for StubBackend {
    fn list_roots(&self, _filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
        Err(BackendError::unsupported())
    }

    fn observe(
        &self,
        _root: &RootInfo,
        _request: ObserveRequest,
    ) -> Result<RootObservation, BackendError> {
        Err(BackendError::unsupported())
    }

    fn perform_action(
        &self,
        _root: &RootInfo,
        _request: &ActionRequest,
    ) -> Result<ActionResult, BackendError> {
        Err(BackendError::unsupported())
    }

    fn read_element_text(
        &self,
        _root: &RootInfo,
        _target_path: &[usize],
    ) -> Result<String, BackendError> {
        Err(BackendError::unsupported())
    }
}
