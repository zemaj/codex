use super::Feature;
use super::Features;
use tracing::info;

#[derive(Clone, Copy)]
struct Alias {
    legacy_key: &'static str,
    feature: Feature,
}

const ALIASES: &[Alias] = &[
    Alias {
        legacy_key: "experimental_sandbox_command_assessment",
        feature: Feature::SandboxCommandAssessment,
    },
    Alias {
        legacy_key: "experimental_use_unified_exec_tool",
        feature: Feature::UnifiedExec,
    },
    Alias {
        legacy_key: "experimental_use_exec_command_tool",
        feature: Feature::StreamableShell,
    },
    Alias {
        legacy_key: "experimental_use_rmcp_client",
        feature: Feature::RmcpClient,
    },
    Alias {
        legacy_key: "experimental_use_freeform_apply_patch",
        feature: Feature::ApplyPatchFreeform,
    },
    Alias {
        legacy_key: "include_apply_patch_tool",
        feature: Feature::ApplyPatchFreeform,
    },
    Alias {
        legacy_key: "include_view_image_tool",
        feature: Feature::ViewImageTool,
    },
    Alias {
        legacy_key: "web_search",
        feature: Feature::WebSearchRequest,
    },
];

pub(crate) fn feature_for_key(key: &str) -> Option<Feature> {
    ALIASES
        .iter()
        .find(|alias| alias.legacy_key == key)
        .map(|alias| {
            log_alias(alias.legacy_key, alias.feature);
            alias.feature
        })
}

#[derive(Debug, Default)]
pub struct LegacyFeatureToggles {
    pub include_apply_patch_tool: Option<bool>,
    pub include_view_image_tool: Option<bool>,
    pub experimental_sandbox_command_assessment: Option<bool>,
    pub experimental_use_freeform_apply_patch: Option<bool>,
    pub experimental_use_exec_command_tool: Option<bool>,
    pub experimental_use_unified_exec_tool: Option<bool>,
    pub experimental_use_rmcp_client: Option<bool>,
    pub tools_web_search: Option<bool>,
    pub tools_view_image: Option<bool>,
}

impl LegacyFeatureToggles {
    pub fn apply(self, features: &mut Features) {
        set_if_some(
            features,
            Feature::ApplyPatchFreeform,
            self.include_apply_patch_tool,
            "include_apply_patch_tool",
        );
        set_if_some(
            features,
            Feature::SandboxCommandAssessment,
            self.experimental_sandbox_command_assessment,
            "experimental_sandbox_command_assessment",
        );
        set_if_some(
            features,
            Feature::ApplyPatchFreeform,
            self.experimental_use_freeform_apply_patch,
            "experimental_use_freeform_apply_patch",
        );
        set_if_some(
            features,
            Feature::StreamableShell,
            self.experimental_use_exec_command_tool,
            "experimental_use_exec_command_tool",
        );
        set_if_some(
            features,
            Feature::UnifiedExec,
            self.experimental_use_unified_exec_tool,
            "experimental_use_unified_exec_tool",
        );
        set_if_some(
            features,
            Feature::RmcpClient,
            self.experimental_use_rmcp_client,
            "experimental_use_rmcp_client",
        );
        set_if_some(
            features,
            Feature::WebSearchRequest,
            self.tools_web_search,
            "tools.web_search",
        );
        set_if_some(
            features,
            Feature::ViewImageTool,
            self.include_view_image_tool,
            "include_view_image_tool",
        );
        set_if_some(
            features,
            Feature::ViewImageTool,
            self.tools_view_image,
            "tools.view_image",
        );
    }
}

fn set_if_some(
    features: &mut Features,
    feature: Feature,
    maybe_value: Option<bool>,
    alias_key: &'static str,
) {
    if let Some(enabled) = maybe_value {
        set_feature(features, feature, enabled);
        log_alias(alias_key, feature);
    }
}

fn set_feature(features: &mut Features, feature: Feature, enabled: bool) {
    if enabled {
        features.enable(feature);
    } else {
        features.disable(feature);
    }
}

fn log_alias(alias: &str, feature: Feature) {
    let canonical = feature.key();
    if alias == canonical {
        return;
    }
    info!(
        %alias,
        canonical,
        "legacy feature toggle detected; prefer `[features].{canonical}`"
    );
}
