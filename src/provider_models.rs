//! Model list resolution: fold a provider's catalog together with the user's
//! per-provider Models settings (favorites, hidden, order, custom slugs).
//!
//! Pure. Both surfaces consume this: the Settings → Providers "Models" section
//! (which shows *everything*, hidden rows included) and the composer's model
//! picker (which drops hidden rows). Keeping one resolver means the picker
//! cannot drift from what the settings page promises.

use agent::{ModelSpec, OptionDescriptor};

use crate::settings::ProviderSettings;

/// The longest custom model slug we accept (T3's validation limit).
pub const MAX_SLUG_LEN: usize = 128;

/// One row of a provider's resolved model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// Provider-native model id (the wire value + the favorites/hidden key).
    pub id: String,
    /// Display name (custom models have none, so they show their slug).
    pub name: String,
    pub favorite: bool,
    pub hidden: bool,
    /// Added by hand in the Models section (removable, not hideable).
    pub custom: bool,
    /// Capability chips from the catalog descriptors: `Fast mode`, `Thinking`,
    /// `Reasoning` (empty for custom models, whose capabilities are unknown).
    pub capabilities: Vec<String>,
}

/// Human-readable capabilities for a catalog model (T3 §4).
fn capabilities(spec: &ModelSpec) -> Vec<String> {
    let mut out = Vec::new();
    for option in &spec.options {
        let label = match option {
            OptionDescriptor::Boolean { id, .. } if id == "fastMode" => {
                rust_i18n::t!("providers.models.cap_fast").into_owned()
            }
            OptionDescriptor::Boolean { id, .. } if id == "thinking" => {
                rust_i18n::t!("providers.models.cap_thinking").into_owned()
            }
            OptionDescriptor::Select { id, .. } if id == "reasoningEffort" => {
                rust_i18n::t!("providers.models.cap_reasoning").into_owned()
            }
            _ => continue,
        };
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// Resolve a provider's full model list: catalog + custom slugs, ordered by the
/// persisted `model_order` (ids not listed keep their catalog order behind the
/// ones that are), then stably partitioned favorites-first.
///
/// Hidden models are *included* here, flagged; call [`picker_models`] for the
/// composer, which drops them.
pub fn resolve_models(
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
    favorites: &[String],
) -> Vec<ResolvedModel> {
    let is_favorite = |id: &str| favorites.iter().any(|f| f == id);
    let is_hidden = |id: &str| settings.hidden_models.iter().any(|h| h == id);

    let mut rows: Vec<ResolvedModel> = catalog
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
    // Custom slugs the catalog doesn't already carry.
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

    // Explicit order first (in `model_order` order), everything else behind it
    // in catalog order. `sort_by_key` is stable, so unlisted ids keep their
    // relative positions.
    let rank = |id: &str| {
        settings
            .model_order
            .iter()
            .position(|m| m == id)
            .unwrap_or(usize::MAX)
    };
    rows.sort_by_key(|row| rank(&row.id));
    // Favorites float to the top of the list, preserving order within groups.
    rows.sort_by_key(|row| !row.favorite);
    rows
}

/// The rows the composer's model picker shows: [`resolve_models`] minus the
/// hidden ones.
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

/// The neighbour a "move up"/"move down" swaps with: the previous/next row
/// *within the same favorite group* (T3 only reorders inside a group).
/// Returns the index to swap with, or `None` when the row is already at the
/// boundary of its group.
pub fn move_target(rows: &[ResolvedModel], index: usize, up: bool) -> Option<usize> {
    let row = rows.get(index)?;
    let candidate = if up {
        index.checked_sub(1)?
    } else {
        index + 1
    };
    let neighbour = rows.get(candidate)?;
    (neighbour.favorite == row.favorite).then_some(candidate)
}

/// Apply a move to the persisted order: rewrite `model_order` to the resulting
/// full id sequence (so later resolutions reproduce it exactly).
pub fn reorder(rows: &[ResolvedModel], from: usize, to: usize) -> Vec<String> {
    let mut ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    if from >= ids.len() || to >= ids.len() {
        return ids;
    }
    let id = ids.remove(from);
    ids.insert(to, id);
    ids
}

/// Why a custom-model slug was rejected (T3's exact validation copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    AlreadyBuiltIn,
    TooLong,
    AlreadySaved,
}

impl SlugError {
    pub fn message(&self) -> String {
        match self {
            SlugError::Empty => rust_i18n::t!("providers.models.err_empty").into_owned(),
            SlugError::AlreadyBuiltIn => {
                rust_i18n::t!("providers.models.err_builtin").into_owned()
            }
            SlugError::TooLong => {
                rust_i18n::t!("providers.models.err_too_long", limit = MAX_SLUG_LEN).into_owned()
            }
            SlugError::AlreadySaved => rust_i18n::t!("providers.models.err_saved").into_owned(),
        }
    }
}

/// Validate a custom model slug against the catalog and the saved customs.
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
    if settings.custom_models.iter().any(|m| m == slug) {
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
        vec![spec("opus", "Opus"), spec("sonnet", "Sonnet"), spec("haiku", "Haiku")]
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
        // Within the favorite group, catalog order (sonnet before haiku) holds.
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["sonnet", "haiku", "opus"]
        );
        assert!(rows[0].favorite && rows[1].favorite && !rows[2].favorite);
    }

    #[test]
    fn explicit_order_wins_over_catalog_order() {
        let settings = ProviderSettings {
            model_order: vec!["haiku".into(), "opus".into()],
            ..ProviderSettings::default()
        };
        let rows = resolve_models(&catalog(), &settings, &[]);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            // Ordered ids first, then the unlisted ones in catalog order.
            ["haiku", "opus", "sonnet"]
        );
    }

    #[test]
    fn hidden_models_stay_in_settings_but_leave_the_picker() {
        let settings = ProviderSettings {
            hidden_models: vec!["sonnet".into()],
            ..ProviderSettings::default()
        };
        let all = resolve_models(&catalog(), &settings, &[]);
        assert_eq!(all.len(), 3);
        assert!(all.iter().find(|r| r.id == "sonnet").unwrap().hidden);

        let picker = picker_models(&catalog(), &settings, &[]);
        assert_eq!(
            picker.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["opus", "haiku"]
        );
    }

    #[test]
    fn custom_models_appear_flagged_and_are_pickable() {
        let settings = ProviderSettings {
            custom_models: vec!["claude-sonnet-5".into()],
            ..ProviderSettings::default()
        };
        let picker = picker_models(&catalog(), &settings, &[]);
        let custom = picker.iter().find(|r| r.id == "claude-sonnet-5").unwrap();
        assert!(custom.custom);
        assert_eq!(custom.name, "claude-sonnet-5");
        // A custom slug can be favorited and hidden like any other row.
        let settings = ProviderSettings {
            hidden_models: vec!["claude-sonnet-5".into()],
            ..settings
        };
        assert!(
            !picker_models(&catalog(), &settings, &[])
                .iter()
                .any(|r| r.id == "claude-sonnet-5")
        );
    }

    #[test]
    fn hidden_order_custom_and_favorites_compose() {
        let settings = ProviderSettings {
            custom_models: vec!["claude-sonnet-5".into()],
            hidden_models: vec!["opus".into()],
            model_order: vec!["claude-sonnet-5".into(), "haiku".into()],
            ..ProviderSettings::default()
        };
        let favorites = vec!["haiku".to_string()];
        let picker = picker_models(&catalog(), &settings, &favorites);
        assert_eq!(
            picker.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            // haiku (favorite) first; then order-listed custom; then the rest.
            // opus is hidden and gone.
            ["haiku", "claude-sonnet-5", "sonnet"]
        );
    }

    #[test]
    fn moves_stay_inside_the_favorite_group() {
        let favorites = vec!["opus".to_string()];
        let rows = resolve_models(&catalog(), &ProviderSettings::default(), &favorites);
        // [opus(fav), sonnet, haiku]
        assert_eq!(move_target(&rows, 0, true), None); // top of its group
        assert_eq!(move_target(&rows, 0, false), None); // next row is a non-favorite
        assert_eq!(move_target(&rows, 1, true), None); // would cross into favorites
        assert_eq!(move_target(&rows, 1, false), Some(2));
        assert_eq!(move_target(&rows, 2, true), Some(1));
        assert_eq!(move_target(&rows, 2, false), None); // bottom
    }

    #[test]
    fn reorder_rewrites_the_full_id_sequence() {
        let rows = resolve_models(&catalog(), &ProviderSettings::default(), &[]);
        assert_eq!(reorder(&rows, 2, 1), ["opus", "haiku", "sonnet"]);
        let settings = ProviderSettings {
            model_order: reorder(&rows, 2, 1),
            ..ProviderSettings::default()
        };
        // The rewritten order round-trips through resolution.
        assert_eq!(
            resolve_models(&catalog(), &settings, &[])
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["opus", "haiku", "sonnet"]
        );
    }

    #[test]
    fn validates_custom_slugs() {
        let settings = ProviderSettings {
            custom_models: vec!["already".into()],
            ..ProviderSettings::default()
        };
        assert_eq!(validate_slug("  ", &catalog(), &settings), Err(SlugError::Empty));
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
            Ok("claude-sonnet-5".to_string())
        );
    }

    #[test]
    fn reads_capabilities_from_catalog_descriptors() {
        let mut spec = spec("opus", "Opus");
        spec.options = vec![
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
        let rows = resolve_models(&[spec], &ProviderSettings::default(), &[]);
        assert_eq!(rows[0].capabilities, ["Thinking", "Reasoning"]);
    }
}
