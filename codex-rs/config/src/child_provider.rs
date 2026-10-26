//! Host-authorized child provider selection, before catalog I/O or authentication.
//!
//! Callers must obtain lineage identity from host-owned thread records, not tool input.
//! This module deliberately never invokes credential helpers or performs network requests.

use crate::ConfigLayerSource;
use crate::ConfigLayerStack;
use crate::config_toml::ChildProviderDefaultsToml;
use crate::config_toml::ConfigToml;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_model_provider_info::built_in_model_providers;
use codex_model_provider_info::merge_configured_model_providers;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct ChildProviderPolicy {
    providers: HashMap<String, ModelProviderInfo>,
    defaults: BTreeMap<String, ChildProviderDefaultsToml>,
    allowed: BTreeSet<String>,
    default_provider: Option<String>,
    required_provider: Option<String>,
    untrusted_provider_ids: BTreeSet<String>,
    unsafe_host_provider_ids: BTreeSet<String>,
    project_roots: Vec<PathBuf>,
    catalog_boundaries: BTreeMap<String, PathBuf>,
    protected_catalog_roots: Vec<PathBuf>,
    file_system_policy: FileSystemSandboxPolicy,
    cwd: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: provider `{provider_id}`: {message}")]
pub struct ChildProviderError {
    pub code: &'static str,
    pub provider_id: String,
    pub message: &'static str,
}

fn error(code: &'static str, provider_id: &str, message: &'static str) -> ChildProviderError {
    ChildProviderError {
        code,
        provider_id: provider_id.to_owned(),
        message,
    }
}

/// Nonsecret host-owned origin record. Storage and lineage lookup are supplied by
/// the thread lifecycle, never by model-visible spawn/resume arguments.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildProviderOriginRecord {
    pub origin_id: Uuid,
    pub root_thread_id: ThreadId,
    pub provider_id: String,
}

/// The origin captured by the host for one root/child's lineage. A resumed child
/// must retain its captured binding even if the root establishes a newer origin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildProviderOriginBinding {
    pub origin_id: Uuid,
    pub root_thread_id: ThreadId,
    pub provider_id: String,
}

/// An origin that matched the host-retained record and captured lineage binding.
/// Deliberately has no public fields or deserializer: model input is not proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChildProviderOrigin(ChildProviderOriginRecord);

impl VerifiedChildProviderOrigin {
    pub fn verify(
        record: &ChildProviderOriginRecord,
        binding: &ChildProviderOriginBinding,
    ) -> Result<Self, ChildProviderError> {
        if record.origin_id.is_nil()
            || record.root_thread_id == ThreadId::from_u128(0)
            || !valid_id(&record.provider_id)
            || record.origin_id != binding.origin_id
            || record.root_thread_id != binding.root_thread_id
            || record.provider_id != binding.provider_id
        {
            return Err(error(
                "child_provider_policy_conflict",
                "unknown",
                "origin record does not match the host-retained lineage binding",
            ));
        }
        Ok(Self(record.clone()))
    }

    pub fn provider_id(&self) -> &str {
        &self.0.provider_id
    }

    pub fn origin_id(&self) -> Uuid {
        self.0.origin_id
    }

    pub fn root_thread_id(&self) -> ThreadId {
        self.0.root_thread_id
    }
}

/// Inputs already captured from the invoking turn and bounded role projection.
/// No field in this type can supply endpoints, credentials, or host authorization.
pub struct ChildProviderRequest<'a> {
    pub explicit_provider: Option<&'a str>,
    pub role_provider: Option<&'a str>,
    pub parent_provider_id: &'a str,
    pub parent_provider: &'a ModelProviderInfo,
    pub parent_model: &'a str,
    /// The host verifies its retained origin record against this child's captured
    /// lineage. None requires current allowlist membership, including on resume.
    pub authorization_origin: Option<VerifiedChildProviderOrigin>,
    pub explicit_model: Option<&'a str>,
    pub role_model: Option<&'a str>,
    pub default_subagent_model: Option<&'a str>,
    pub full_history: bool,
    /// Internal safety/control children must not consume delegated provider preferences.
    pub host_owned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedChildProvider {
    pub provider_id: String,
    pub provider: ModelProviderInfo,
    pub model: String,
    pub model_catalog_json: Option<AbsolutePathBuf>,
    /// Provider ID changed. This is not a complete history-compatibility check.
    pub changed_provider: bool,
}

impl ChildProviderPolicy {
    /// Capture only enabled host-owned layers. Project/session authority cannot leak in
    /// through a provider definition, a provider default, or a built-in endpoint override.
    /// `protected_catalog_roots` must be actual read-only carveouts from the finalized
    /// host filesystem policy for this cwd, never merely trusted config directories.
    /// Pass that same finalized policy: narrower write grants can reopen a carveout.
    pub fn from_config_layers(
        stack: &ConfigLayerStack,
        cwd: &AbsolutePathBuf,
        workspace_roots: &[AbsolutePathBuf],
        protected_catalog_roots: &[AbsolutePathBuf],
        file_system_policy: &FileSystemSandboxPolicy,
    ) -> std::io::Result<Self> {
        let mut project_roots = vec![cwd.to_path_buf()];
        project_roots.extend(workspace_roots.iter().map(|root| root.to_path_buf()));
        for layer in stack.layers_low_to_high() {
            if let ConfigLayerSource::Project { dot_codex_folder } = &layer.name
                && let Some(root) = dot_codex_folder.as_path().parent()
            {
                project_roots.push(root.to_path_buf());
            }
        }
        let trusted_source = |source: &ConfigLayerSource| {
            is_host_source(source)
                && source_file(source).is_none_or(|file| {
                    canonical_source_path(file).is_some_and(|file| {
                        !project_roots.iter().any(|root| {
                            root.canonicalize().is_ok_and(|root| file.starts_with(root))
                        }) || (!file_system_policy
                            .can_write_local_path_with_cwd(&file, cwd.as_path())
                            && protected_catalog_roots.iter().any(|root| {
                                root.as_path()
                                    .canonicalize()
                                    .is_ok_and(|root| file.starts_with(root))
                            }))
                    })
                })
        };
        // Managed restrictions may never disappear because their backing file is
        // writable or cannot be resolved. Reject that authority instead.
        for layer in stack.layers_low_to_high() {
            if is_managed_source(&layer.name)
                && !trusted_source(&layer.name)
                && has_child_provider_authority(&layer.config)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed child-provider authority has an untrusted file source",
                ));
            }
        }
        // Parse only the authority owned by this resolver. Unrelated trusted
        // settings may legitimately be corrected by a higher project layer.
        let mut effective = toml::Value::Table(toml::map::Map::new());
        for layer in stack
            .layers_low_to_high()
            .filter(|layer| trusted_source(&layer.name))
        {
            let mut projected = project_child_provider_config(&layer.config);
            if let Some(directory) = source_directory(&layer.name) {
                projected =
                    crate::loader::resolve_relative_paths_in_config_toml(projected, directory)?;
            }
            crate::merge::merge_toml_values(&mut effective, &projected);
        }
        let required = crate::model_provider_requirements::to_config(stack.requirements_toml())?;
        crate::model_provider_requirements::apply(&mut effective, &required);
        let config: ConfigToml = effective.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid trusted child-provider configuration",
            )
        })?;
        let agents = config.agents.unwrap_or_default();
        let allowed = agents.allowed_model_providers.unwrap_or_default();
        if allowed.iter().any(|id| !valid_id(id))
            || agents
                .default_subagent_model_provider
                .as_deref()
                .is_some_and(|id| !valid_id(id))
            || agents.provider_defaults.keys().any(|id| !valid_id(id))
            || agents.provider_defaults.values().any(|defaults| {
                defaults
                    .model
                    .as_deref()
                    .is_some_and(|model| model.trim().is_empty())
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "child-provider IDs and configured models must be nonempty and IDs must have no surrounding whitespace",
            ));
        }
        let providers = merge_configured_model_providers(
            built_in_model_providers(config.openai_base_url),
            config.model_providers,
        )
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid trusted child-provider definitions",
            )
        })?;
        let mut allowed: BTreeSet<_> = allowed.into_iter().collect();
        let mut untrusted_provider_ids = BTreeSet::new();
        let mut unsafe_host_provider_ids = BTreeSet::new();
        let mut catalog_boundaries = BTreeMap::new();
        for layer in stack.layers_low_to_high() {
            if trusted_source(&layer.name) {
                if is_managed_source(&layer.name)
                    && let Some(value) = layer
                        .config
                        .get("agents")
                        .and_then(|agents| agents.get("allowed_model_providers"))
                {
                    let managed: Vec<String> = value.clone().try_into().map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid managed child-provider allowlist",
                        )
                    })?;
                    if managed.iter().any(|id| !valid_id(id)) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid managed child-provider ID",
                        ));
                    }
                    let managed: BTreeSet<_> = managed.into_iter().collect();
                    allowed.retain(|id| managed.contains(id));
                }
                if let Some(defaults) = layer
                    .config
                    .get("agents")
                    .and_then(|agents| agents.get("provider_defaults"))
                    .and_then(toml::Value::as_table)
                {
                    for (id, entry) in defaults {
                        if entry.get("model_catalog_json").is_some() {
                            catalog_boundaries.remove(id);
                            if let Some(directory) = source_directory(&layer.name) {
                                catalog_boundaries.insert(id.clone(), directory.to_path_buf());
                            }
                        }
                    }
                }
                continue;
            }
            if let Some(table) = layer
                .config
                .get("model_providers")
                .and_then(toml::Value::as_table)
            {
                untrusted_provider_ids.extend(table.keys().cloned());
                if is_host_source(&layer.name) {
                    unsafe_host_provider_ids.extend(table.keys().cloned());
                }
            }
            if layer.config.get("openai_base_url").is_some() {
                untrusted_provider_ids.insert(OPENAI_PROVIDER_ID.to_owned());
                if is_host_source(&layer.name) {
                    unsafe_host_provider_ids.insert(OPENAI_PROVIDER_ID.to_owned());
                }
            }
        }
        Ok(Self {
            providers,
            defaults: agents.provider_defaults,
            allowed,
            default_provider: agents.default_subagent_model_provider,
            required_provider: stack.required_model_provider().map(str::to_owned),
            untrusted_provider_ids,
            unsafe_host_provider_ids,
            project_roots,
            catalog_boundaries,
            file_system_policy: file_system_policy.clone(),
            cwd: cwd.clone(),
            protected_catalog_roots: protected_catalog_roots
                .iter()
                .map(|root| root.to_path_buf())
                .collect(),
        })
    }

    pub fn default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    /// Authorization stage for warm/cold resume and subsequent requests. Re-resolve
    /// definitions/catalogs after configuration refresh before rebuilding transport.
    /// Equality with the immediate parent is not an exemption from revocation.
    pub fn authorize(
        &self,
        provider_id: &str,
        origin: Option<&VerifiedChildProviderOrigin>,
    ) -> Result<(), ChildProviderError> {
        let verified_root_id = origin.map(VerifiedChildProviderOrigin::provider_id);
        if self
            .required_provider
            .as_deref()
            .is_some_and(|id| id != provider_id)
        {
            return Err(error(
                "child_provider_policy_conflict",
                provider_id,
                "managed policy requires a different provider",
            ));
        }
        // A reloaded writable host file must not replace even a root-inherited
        // definition. Root legacy inheritance covers project/session selection,
        // not newly promoted file-backed authority.
        if self.unsafe_host_provider_ids.contains(provider_id) {
            return Err(error(
                "child_provider_untrusted_source",
                provider_id,
                "the host provider definition has an untrusted file source",
            ));
        }

        if verified_root_id != Some(provider_id) && !self.allowed.contains(provider_id) {
            return Err(error(
                "child_provider_not_authorized",
                provider_id,
                "host authorization is required or has been revoked",
            ));
        }
        if verified_root_id != Some(provider_id) && !self.providers.contains_key(provider_id) {
            return Err(error(
                "child_provider_unknown",
                provider_id,
                "configure this provider in a host-owned layer",
            ));
        }
        Ok(())
    }

    /// Resolve identity/model/catalog together without performing metadata or auth I/O.
    /// After this succeeds, callers validate the selected model using only that
    /// provider's catalog, via `ResolvedChildProvider::validate_model`.
    pub fn resolve(
        &self,
        request: ChildProviderRequest<'_>,
    ) -> Result<ResolvedChildProvider, ChildProviderError> {
        if request.host_owned {
            if request.explicit_provider.is_some() || request.role_provider.is_some() {
                return Err(error(
                    "child_provider_policy_conflict",
                    request.parent_provider_id,
                    "host-owned children do not accept delegated provider preferences",
                ));
            }
            // The caller supplies the host-policy-selected configuration. Neither
            // the delegated allowlist nor delegated model/catalog defaults apply.
            return Ok(ResolvedChildProvider {
                provider_id: request.parent_provider_id.to_owned(),
                provider: request.parent_provider.clone(),
                model: request.parent_model.to_owned(),
                model_catalog_json: None,
                changed_provider: false,
            });
        }
        let provider_id = request
            .explicit_provider
            .or(request.role_provider)
            .or(self.default_provider())
            .unwrap_or(request.parent_provider_id);
        let changed_provider = provider_id != request.parent_provider_id;
        self.authorize(provider_id, request.authorization_origin.as_ref())?;
        if changed_provider && request.full_history {
            return Err(error(
                "child_provider_history_unsupported",
                provider_id,
                "cross-provider full history is unsupported; use a fresh child",
            ));
        }
        // The root's existing same-provider behavior remains unchanged, including
        // trusted-at-root legacy session configuration. Switching never uses it.
        let legacy_inheritance = !changed_provider
            && request
                .authorization_origin
                .as_ref()
                .map(VerifiedChildProviderOrigin::provider_id)
                == Some(provider_id);
        if !legacy_inheritance && self.untrusted_provider_ids.contains(provider_id) {
            return Err(error(
                "child_provider_untrusted_source",
                provider_id,
                "a project or session layer shadows this provider definition",
            ));
        }
        let provider = if legacy_inheritance {
            request.parent_provider.clone()
        } else {
            self.providers.get(provider_id).cloned().ok_or_else(|| {
                error(
                    "child_provider_unknown",
                    provider_id,
                    "configure this provider in a host-owned layer",
                )
            })?
        };
        if request.full_history && provider != *request.parent_provider {
            return Err(error(
                "child_provider_history_unsupported",
                provider_id,
                "provider definition changed; use a fresh child",
            ));
        }
        let defaults = self.defaults.get(provider_id);
        let model = request
            .role_model
            .or(request.explicit_model)
            .or_else(|| {
                if changed_provider {
                    defaults.and_then(|defaults| defaults.model.as_deref())
                } else {
                    request
                        .default_subagent_model
                        .or(Some(request.parent_model))
                }
            })
            .filter(|model| !model.trim().is_empty())
            .ok_or_else(|| {
                error(
                    "child_provider_model_required",
                    provider_id,
                    "select a model or configure a target provider model default",
                )
            })?;
        let model_catalog_json = defaults
            .and_then(|defaults| defaults.model_catalog_json.as_ref())
            .map(|path| self.validate_catalog_path(provider_id, path.as_path()))
            .transpose()?;
        if changed_provider
            && model_catalog_json.is_none()
            && provider.model_catalog_url.is_none()
            && provider_id != OPENAI_PROVIDER_ID
        {
            return Err(error(
                "child_provider_model_unavailable",
                provider_id,
                "configure an authoritative provider model catalog",
            ));
        }
        Ok(ResolvedChildProvider {
            provider_id: provider_id.to_owned(),
            provider,
            model: model.to_owned(),
            model_catalog_json,
            changed_provider,
        })
    }

    fn validate_catalog_path(
        &self,
        provider_id: &str,
        path: &Path,
    ) -> Result<AbsolutePathBuf, ChildProviderError> {
        let canonical = path.canonicalize().map_err(|_| {
            error(
                "child_provider_model_unavailable",
                provider_id,
                "configured catalog file is unavailable",
            )
        })?;
        if !canonical.is_file() {
            return Err(error(
                "child_provider_model_unavailable",
                provider_id,
                "configured catalog is not a file",
            ));
        }
        let boundary = self
            .catalog_boundaries
            .get(provider_id)
            .and_then(|boundary| boundary.canonicalize().ok());
        let protected_boundary = !self
            .file_system_policy
            .can_write_local_path_with_cwd(&canonical, self.cwd.as_path())
            && boundary.as_ref().is_some_and(|boundary| {
                self.protected_catalog_roots.iter().any(|root| {
                    root.canonicalize()
                        .is_ok_and(|root| boundary.starts_with(root))
                })
            });
        // An incidental ancestor repository does not turn a declaring host directory
        // into project configuration. Active workspace roots are stricter: the host
        // directory must have an actual read-only permission carveout. Repositories
        // at/below the declaring directory still count as project controlled.
        let controls_catalog = |root: &Path| {
            canonical.starts_with(root)
                && !boundary.as_ref().is_some_and(|boundary| {
                    boundary != root
                        && boundary.starts_with(root)
                        && canonical.starts_with(boundary)
                })
        };
        if self.project_roots.iter().any(|root| {
            root.canonicalize().is_ok_and(|root| {
                canonical.starts_with(&root) && !(protected_boundary && !controls_catalog(&root))
            })
        }) || canonical
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists() && controls_catalog(ancestor))
        {
            return Err(error(
                "child_provider_untrusted_source",
                provider_id,
                "catalog resolves inside a project-controlled directory",
            ));
        }
        AbsolutePathBuf::from_absolute_path(canonical).map_err(|_| {
            error(
                "child_provider_untrusted_source",
                provider_id,
                "catalog path could not be resolved to an absolute host path",
            )
        })
    }
}

impl ResolvedChildProvider {
    /// Catalog loading belongs to the provider-scoped models manager. Do not pass
    /// parent fallback metadata here: the exact selected slug must be present.
    pub fn validate_model<'a>(
        &self,
        catalog_provider_id: &str,
        catalog: &'a [ModelInfo],
    ) -> Result<&'a ModelInfo, ChildProviderError> {
        if catalog_provider_id != self.provider_id {
            return Err(error(
                "child_provider_model_unavailable",
                &self.provider_id,
                "catalog belongs to a different provider",
            ));
        }
        catalog
            .iter()
            .find(|model| model.slug == self.model && !model.used_fallback_model_metadata)
            .ok_or_else(|| {
                error(
                    "child_provider_model_unavailable",
                    &self.provider_id,
                    "selected model is absent from the authoritative provider catalog",
                )
            })
    }
}

fn project_child_provider_config(config: &toml::Value) -> toml::Value {
    let mut projected = toml::map::Map::new();
    for key in ["openai_base_url", "model_providers"] {
        if let Some(value) = config.get(key) {
            projected.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(agents) = config.get("agents").and_then(toml::Value::as_table) {
        let mut child_settings = toml::map::Map::new();
        for key in [
            "allowed_model_providers",
            "default_subagent_model_provider",
            "provider_defaults",
        ] {
            if let Some(value) = agents.get(key) {
                child_settings.insert(key.to_owned(), value.clone());
            }
        }
        projected.insert("agents".to_owned(), toml::Value::Table(child_settings));
    }
    toml::Value::Table(projected)
}

fn is_host_source(source: &ConfigLayerSource) -> bool {
    !matches!(
        source,
        ConfigLayerSource::Project { .. } | ConfigLayerSource::SessionFlags
    )
}

fn is_managed_source(source: &ConfigLayerSource) -> bool {
    matches!(
        source,
        ConfigLayerSource::Mdm { .. }
            | ConfigLayerSource::EnterpriseManaged { .. }
            | ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. }
            | ConfigLayerSource::LegacyManagedConfigTomlFromMdm
    )
}

fn has_child_provider_authority(config: &toml::Value) -> bool {
    let projected = project_child_provider_config(config);
    projected.as_table().is_some_and(|table| {
        table.iter().any(|(key, value)| {
            key != "agents" || value.as_table().is_none_or(|agents| !agents.is_empty())
        })
    })
}

fn canonical_source_path(file: &Path) -> Option<PathBuf> {
    // Optional/synthetic missing config files have no symlink target. Resolve
    // their existing parent; any other filesystem error fails closed.
    match file.canonicalize() {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(file.parent()?.canonicalize().ok()?.join(file.file_name()?))
        }
        Err(_) => None,
    }
}

fn source_file(source: &ConfigLayerSource) -> Option<&Path> {
    match source {
        ConfigLayerSource::PackagedDefaults { file }
        | ConfigLayerSource::System { file }
        | ConfigLayerSource::User { file, .. }
        | ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => Some(file.as_path()),
        _ => None,
    }
}

fn source_directory(source: &ConfigLayerSource) -> Option<&Path> {
    match source {
        ConfigLayerSource::PackagedDefaults { file }
        | ConfigLayerSource::System { file }
        | ConfigLayerSource::User { file, .. }
        | ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => file.as_path().parent(),
        _ => None,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id == id.trim() && !id.chars().any(char::is_control)
}

#[cfg(test)]
#[path = "child_provider_tests.rs"]
mod tests;
