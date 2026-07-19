//! Pure provider model resolution, ordering, and custom-slug validation.

use agent::{ModelSpec, OptionDescriptor};

use crate::settings::ProviderSettings;

pub const MAX_SLUG_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCapability {
    FastMode,
    Thinking,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: String,
    pub name: String,
    pub favorite: bool,
    pub hidden: bool,
    pub custom: bool,
    pub capabilities: Vec<ModelCapability>,
}

fn capabilities(spec: &ModelSpec) -> Vec<ModelCapability> {
    let mut out = Vec::new();
    for option in &spec.options {
        let capability = match option {
            OptionDescriptor::Boolean { id, .. } if id == "fastMode" => ModelCapability::FastMode,
            OptionDescriptor::Boolean { id, .. } if id == "thinking" => ModelCapability::Thinking,
            OptionDescriptor::Select { id, .. } if id == "reasoningEffort" => {
                ModelCapability::Reasoning
            }
            _ => continue,
        };
        if !out.contains(&capability) {
            out.push(capability);
        }
    }
    out
}

pub fn resolve_models(
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
    favorites: &[String],
) -> Vec<ResolvedModel> {
    let is_favorite = |id: &str| favorites.iter().any(|f| f == id);
    let is_hidden = |id: &str| settings.hidden_models.iter().any(|h| h == id);
    let mut rows: Vec<_> = catalog
        .iter()
        .map(|spec| ResolvedModel {
            id: spec.id.clone(),
            name: spec.display_name.clone(),
            favorite: is_favorite(&spec.id),
            hidden: is_hidden(&spec.id),
            custom: false,
            capabilities: capabilities(spec),
        })
        .collect();
    for slug in &settings.custom_models {
        if rows.iter().any(|row| &row.id == slug) {
            continue;
        }
        rows.push(ResolvedModel {
            id: slug.clone(),
            name: slug.clone(),
            favorite: is_favorite(slug),
            hidden: is_hidden(slug),
            custom: true,
            capabilities: Vec::new(),
        });
    }
    // Favorites float to the top; everything else keeps its catalog order.
    rows.sort_by_key(|row| !row.favorite);
    rows
}

pub fn picker_models(
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
    favorites: &[String],
) -> Vec<ResolvedModel> {
    resolve_models(catalog, settings, favorites)
        .into_iter()
        .filter(|row| !row.hidden)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    AlreadyBuiltIn,
    TooLong,
    AlreadySaved,
}

pub fn validate_slug(
    raw: &str,
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
) -> Result<String, SlugError> {
    let slug = raw.trim();
    if slug.is_empty() {
        return Err(SlugError::Empty);
    }
    if slug.chars().count() > MAX_SLUG_LEN {
        return Err(SlugError::TooLong);
    }
    if catalog.iter().any(|spec| spec.id == slug) {
        return Err(SlugError::AlreadyBuiltIn);
    }
    if settings.custom_models.iter().any(|model| model == slug) {
        return Err(SlugError::AlreadySaved);
    }
    Ok(slug.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{OptionDescriptor, SelectOption};

    fn spec(id: &str, name: &str) -> ModelSpec {
        ModelSpec {
            id: id.into(),
            display_name: name.into(),
            is_default: false,
            options: Vec::new(),
        }
    }

    fn catalog() -> Vec<ModelSpec> {
        vec![
            spec("opus", "Opus"),
            spec("sonnet", "Sonnet"),
            spec("haiku", "Haiku"),
        ]
    }

    #[test]
    fn resolves_catalog_order_by_default() {
        let rows = resolve_models(&catalog(), &ProviderSettings::default(), &[]);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["opus", "sonnet", "haiku"]
        );
        assert!(rows.iter().all(|r| !r.hidden && !r.custom && !r.favorite));
    }

    #[test]
    fn favorites_float_to_the_top_and_keep_relative_order() {
        let favorites = vec!["haiku".to_string(), "sonnet".to_string()];
        let rows = resolve_models(&catalog(), &ProviderSettings::default(), &favorites);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["sonnet", "haiku", "opus"]
        );
        assert!(rows[0].favorite && rows[1].favorite && !rows[2].favorite);
    }

    #[test]
    fn hidden_models_stay_in_settings_but_leave_the_picker() {
        let settings = ProviderSettings {
            hidden_models: vec!["sonnet".into()],
            ..Default::default()
        };
        let all = resolve_models(&catalog(), &settings, &[]);
        assert_eq!(all.len(), 3);
        assert!(all.iter().find(|row| row.id == "sonnet").unwrap().hidden);
        assert_eq!(
            picker_models(&catalog(), &settings, &[])
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["opus", "haiku"]
        );
    }

    #[test]
    fn custom_models_appear_flagged_and_are_pickable() {
        let settings = ProviderSettings {
            custom_models: vec!["claude-sonnet-5".into()],
            ..Default::default()
        };
        let picker = picker_models(&catalog(), &settings, &[]);
        let custom = picker.iter().find(|r| r.id == "claude-sonnet-5").unwrap();
        assert!(custom.custom);
        assert_eq!(custom.name, "claude-sonnet-5");
        let settings = ProviderSettings {
            hidden_models: vec!["claude-sonnet-5".into()],
            ..settings
        };
        assert!(
            !picker_models(&catalog(), &settings, &[])
                .iter()
                .any(|row| row.id == "claude-sonnet-5")
        );
    }

    #[test]
    fn hidden_custom_and_favorites_compose() {
        let settings = ProviderSettings {
            custom_models: vec!["claude-sonnet-5".into()],
            hidden_models: vec!["opus".into()],
            ..Default::default()
        };
        let rows = picker_models(&catalog(), &settings, &["haiku".into()]);
        // Favorite floats up; the rest keep catalog order (custom slugs last);
        // hidden `opus` is filtered from the picker.
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["haiku", "sonnet", "claude-sonnet-5"]
        );
    }

    #[test]
    fn validates_custom_slugs() {
        let settings = ProviderSettings {
            custom_models: vec!["already".into()],
            ..Default::default()
        };
        assert_eq!(
            validate_slug("  ", &catalog(), &settings),
            Err(SlugError::Empty)
        );
        assert_eq!(
            validate_slug("opus", &catalog(), &settings),
            Err(SlugError::AlreadyBuiltIn)
        );
        assert_eq!(
            validate_slug("already", &catalog(), &settings),
            Err(SlugError::AlreadySaved)
        );
        assert_eq!(
            validate_slug(&"x".repeat(MAX_SLUG_LEN + 1), &catalog(), &settings),
            Err(SlugError::TooLong)
        );
        assert_eq!(
            validate_slug("  claude-sonnet-5 ", &catalog(), &settings),
            Ok("claude-sonnet-5".into())
        );
    }

    #[test]
    fn reads_capabilities_from_catalog_descriptors() {
        let mut model = spec("opus", "Opus");
        model.options = vec![
            OptionDescriptor::Boolean {
                id: "thinking".into(),
                label: "Thinking".into(),
                default_value: false,
            },
            OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                options: vec![SelectOption {
                    value: "high".into(),
                    label: "High".into(),
                    description: None,
                }],
                default_value: None,
            },
        ];
        assert_eq!(
            resolve_models(&[model], &ProviderSettings::default(), &[])[0].capabilities,
            [ModelCapability::Thinking, ModelCapability::Reasoning]
        );
    }
}
