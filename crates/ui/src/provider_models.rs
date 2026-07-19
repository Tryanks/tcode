//! Provider model presentation with core-owned semantics and localized copy.

pub use tcode_core::provider_models::{
    MAX_SLUG_LEN, ModelCapability, ResolvedModel, SlugError, picker_models, resolve_models,
    validate_slug,
};

pub fn model_capability_label(capability: ModelCapability) -> String {
    match capability {
        ModelCapability::FastMode => tcode_i18n::tr!("providers.models.cap_fast").into_owned(),
        ModelCapability::Thinking => tcode_i18n::tr!("providers.models.cap_thinking").into_owned(),
        ModelCapability::Reasoning => {
            tcode_i18n::tr!("providers.models.cap_reasoning").into_owned()
        }
    }
}

pub fn slug_error_message(error: &SlugError) -> String {
    match error {
        SlugError::Empty => tcode_i18n::tr!("providers.models.err_empty").into_owned(),
        SlugError::AlreadyBuiltIn => tcode_i18n::tr!("providers.models.err_builtin").into_owned(),
        SlugError::TooLong => {
            tcode_i18n::tr!("providers.models.err_too_long", limit = MAX_SLUG_LEN).into_owned()
        }
        SlugError::AlreadySaved => tcode_i18n::tr!("providers.models.err_saved").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_presentation_copy_is_preserved() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        assert_eq!(
            model_capability_label(ModelCapability::FastMode),
            "Fast mode"
        );
        assert_eq!(
            model_capability_label(ModelCapability::Thinking),
            "Thinking"
        );
        assert_eq!(
            model_capability_label(ModelCapability::Reasoning),
            "Reasoning"
        );
        assert_eq!(slug_error_message(&SlugError::Empty), "Enter a model slug.");
        assert_eq!(
            slug_error_message(&SlugError::AlreadyBuiltIn),
            "That model is already built in."
        );
        assert_eq!(
            slug_error_message(&SlugError::TooLong),
            "Model slugs must be 128 characters or less."
        );
        assert_eq!(
            slug_error_message(&SlugError::AlreadySaved),
            "That custom model is already saved."
        );
    }
}
