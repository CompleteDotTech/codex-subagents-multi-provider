use super::cache::FileModelsCache;
use crate::cache::ModelsCache;
use crate::cache::ModelsCacheEntry;
use crate::collaboration_mode_presets::builtin_collaboration_mode_presets;
use crate::config::ModelsManagerConfig;
use crate::model_info;
use chrono::DateTime;
use chrono::Utc;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tracing::Instrument as _;
use tracing::error;
use tracing::info;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Nonblocking in-memory provider identity; never an authority-record payload.
#[derive(Clone, Debug)]
pub struct PinnedModelsIdentity {
    pub identity: String,
    pub uses_api_key_auth: bool,
}

/// Remote endpoint used by the OpenAI-compatible model manager.
///
/// Implementations own provider-specific auth and transport details. The model
/// manager owns refresh policy, cache behavior, and catalog merging; it calls
/// this endpoint only when it decides a remote refresh should happen.
pub trait ModelsEndpointClient: fmt::Debug + Send + Sync {
    /// Observe externally replaceable credential state before serving derived
    /// metadata. Ordinary providers retain the cheap existing identity path.
    fn reconcile_identity(&self) -> ModelsEndpointFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }

    /// Unknown implementations cannot establish pure authority snapshots.
    fn try_pinned_identity(&self) -> Option<PinnedModelsIdentity> {
        None
    }

    /// Synchronous metadata readers cannot perform the required store observation.
    fn requires_identity_reconciliation(&self) -> bool {
        false
    }
    /// Opaque identity of the current provider and credentials, without resolving auth.
    ///
    /// Must distinguish provider routing and authenticated principals within the
    /// manager and cache's caller-established scope.
    /// Return `None` when identity is unavailable; cached catalog reuse is then disabled.
    fn identity(&self) -> Option<String>;

    /// Returns whether this provider can authenticate command-scoped requests.
    fn has_command_auth(&self) -> bool;

    /// Returns whether the currently resolved auth can use Codex backend-only models.
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool>;

    /// Returns whether this provider supports an authoritative catalog with OpenAI API keys.
    fn supports_api_key_models(&self) -> bool {
        false
    }

    /// Returns whether explicit provider configuration supplies API-key authentication.
    /// This takes precedence over any unrelated first-party login used by the picker.
    fn has_provider_api_key(&self) -> bool {
        false
    }

    /// Fetches the latest remote model catalog and optional ETag.
    fn list_models<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, Result<ModelsEndpointResponse, ModelsEndpointError>>;
}

/// Catalog and validation metadata captured using the credentials for one request.
#[derive(Debug)]
pub struct ModelsEndpointResponse {
    pub models: Vec<ModelInfo>,
    pub etag: Option<String>,
    /// Identity of the credentials actually used to fetch this catalog.
    /// Successful responses must always identify their provider and authentication scope.
    pub identity: String,
}

/// Failure scoped to the credentials actually resolved for one attempted request.
#[derive(Debug)]
pub struct ModelsEndpointError {
    pub error: codex_protocol::error::CodexErr,
    /// None means authentication failed before an attempted identity was established.
    pub identity: Option<String>,
}
impl From<codex_protocol::error::CodexErr> for ModelsEndpointError {
    fn from(error: codex_protocol::error::CodexErr) -> Self {
        Self {
            error,
            identity: None,
        }
    }
}
impl std::ops::Deref for ModelsEndpointError {
    type Target = codex_protocol::error::CodexErr;
    fn deref(&self) -> &Self::Target {
        &self.error
    }
}
impl fmt::Display for ModelsEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

pub type ModelsEndpointFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from the network, ignoring cache.
    Online,
    /// Only use cached data, never fetch from the network.
    Offline,
    /// Use cache if available and fresh, otherwise fetch from the network.
    OnlineIfUncached,
}

impl RefreshStrategy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::OnlineIfUncached => "online_if_uncached",
        }
    }
}

impl fmt::Display for RefreshStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

type SharedModelsEndpointClient = Arc<dyn ModelsEndpointClient>;

/// Coordinates model discovery plus cached metadata on disk.
pub trait ModelsManager: fmt::Debug + Send + Sync {
    /// Supply startup API-key discovery policy; live changes require a new session.
    /// Static catalogs ignore this setting.
    fn set_api_key_model_discovery_enabled(&self, _enabled: bool) {}

    /// List all available models, refreshing according to the specified strategy.
    ///
    /// Returns model presets sorted by priority and filtered by auth mode and visibility.
    fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        Box::pin(
            async move {
                let catalog = self
                    .raw_model_catalog(refresh_strategy, http_client_factory)
                    .await;
                self.build_available_models(catalog.models)
            }
            .instrument(tracing::info_span!(
                "list_models",
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    /// Return the active raw model catalog, refreshing according to the specified strategy.
    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse>;

    /// Best-effort refresh when the in-memory catalog belongs to different credentials.
    /// Static catalogs need no refresh. Failures follow the catalog's fallback policy.
    fn refresh_after_auth_change(
        &self,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }

    /// Return the current in-memory remote model catalog without refreshing or loading cache state.
    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>>;

    /// Attempt to return the current in-memory remote model catalog without blocking.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError>;

    /// Pure snapshot of already validated in-memory metadata for host authority
    /// receipts. Never reconciles credentials, loads disk, or refreshes transport.
    /// Missing/busy/stale/fallback metadata cannot establish an authority receipt.
    fn try_pinned_authority_model(
        &self,
        _model: &str,
        _config: &ModelsManagerConfig,
    ) -> Option<ModelInfo> {
        None
    }

    /// Return the auth manager used for picker filtering.
    fn auth_manager(&self) -> Option<&AuthManager>;

    /// Build picker-ready presets from the active catalog snapshot.
    fn build_available_models(&self, mut remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        remote_models.sort_by_key(|model| model.priority);

        let mut presets: Vec<ModelPreset> = remote_models.into_iter().map(Into::into).collect();
        let uses_codex_backend = self
            .auth_manager()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        presets = ModelPreset::filter_by_auth(presets, uses_codex_backend);

        ModelPreset::mark_default_by_picker_visibility(&mut presets);

        presets
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask>;

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        let remote_models = self.try_get_remote_models()?;
        Ok(self.build_available_models(remote_models))
    }

    // todo(aibrahim): should be visible to core only and sent on session_configured event
    /// Get the model identifier to use, refreshing according to the specified strategy.
    ///
    /// If `model` is provided, preserves it unless the implementation supports and the policy
    /// allows provider fallback. Otherwise selects the default based on auth mode and available
    /// models.
    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        allow_provider_model_fallback: bool,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'a, String> {
        Box::pin(
            async move {
                if let Some(model) = model.as_ref() {
                    return model.to_string();
                }
                default_model_from_available(
                    self.list_models(refresh_strategy, http_client_factory)
                        .await,
                )
            }
            .instrument(tracing::info_span!(
                "get_default_model",
                model.provided = model.is_some(),
                allow_provider_model_fallback,
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    // todo(aibrahim): look if we can tighten it to pub(crate)
    /// Look up model metadata, applying remote overrides and config adjustments.
    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(
            async move {
                let remote_models = self.get_remote_models().await;
                construct_model_info_from_candidates(model, &remote_models, config)
            }
            .instrument(tracing::info_span!("get_model_info", model = model)),
        )
    }

    /// Refresh models if the provided ETag differs from the cached ETag.
    ///
    /// Uses `Online` strategy to fetch latest models when ETags differ.
    fn refresh_if_new_etag(
        &self,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()>;
}

pub type ModelsManagerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shared model manager handle used across runtime services.
pub type SharedModelsManager = Arc<dyn ModelsManager>;

/// Only explicit catalogs serialize refresh and suppress bundled fallback. Auth and discovery
/// settings are still evaluated when models are requested.
#[derive(Debug)]
enum CatalogSource {
    Default,
    ExplicitProvider(Mutex<()>),
}

impl CatalogSource {
    fn fallback_models(&self) -> Option<Vec<ModelInfo>> {
        match self {
            Self::Default => Some(load_remote_models_from_file().unwrap_or_default()),
            Self::ExplicitProvider(_) => None,
        }
    }

    async fn refresh_lock(&self) -> Option<MutexGuard<'_, ()>> {
        match self {
            Self::Default => None,
            Self::ExplicitProvider(lock) => Some(lock.lock().await),
        }
    }
}

/// OpenAI-compatible model manager backed by bundled models, cache, and `/models`.
#[derive(Debug)]
pub struct OpenAiModelsManager {
    remote_models: RwLock<ModelsCacheEntry>,
    rejected_identities: std::sync::RwLock<HashSet<String>>,
    cache: Option<Arc<dyn ModelsCache>>,
    endpoint_client: SharedModelsEndpointClient,
    catalog_source: CatalogSource,
    api_key_model_discovery_enabled: AtomicBool,
    auth_manager: Option<Arc<AuthManager>>,
}

/// Static model manager backed by an authoritative in-process catalog.
#[derive(Debug)]
pub struct StaticModelsManager {
    remote_models: Vec<ModelInfo>,
    auth_manager: Option<Arc<AuthManager>>,
}

impl OpenAiModelsManager {
    /// Construct an OpenAI-compatible remote model manager.
    pub fn new(
        codex_home: PathBuf,
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        Self::new_with_optional_cache(
            Some(Arc::new(FileModelsCache::partitioned(
                cache_path,
                DEFAULT_MODEL_CACHE_TTL,
            ))),
            endpoint_client,
            auth_manager,
        )
    }

    /// Construct an OpenAI-compatible model manager with caching disabled.
    pub fn new_without_cache(
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self::new_with_optional_cache(/*cache*/ None, endpoint_client, auth_manager)
    }

    /// Constructs an OpenAI-compatible model manager with a caller-provided cache.
    ///
    /// The cache is consulted by cache-aware refresh strategies. Cache misses and backend errors
    /// fall back to the models endpoint, and cache write failures do not fail model discovery.
    pub fn new_with_cache(
        cache: Arc<dyn ModelsCache>,
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self::new_with_optional_cache(Some(cache), endpoint_client, auth_manager)
    }

    fn new_with_optional_cache(
        cache: Option<Arc<dyn ModelsCache>>,
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            remote_models: RwLock::new(ModelsCacheEntry {
                fetched_at: Utc::now(),
                etag: None,
                client_version: Some(crate::client_version_to_whole()),
                identity: endpoint_client.identity(),
                models: load_remote_models_from_file().unwrap_or_default(),
            }),
            rejected_identities: std::sync::RwLock::new(HashSet::new()),
            cache,
            api_key_model_discovery_enabled: AtomicBool::new(false),
            endpoint_client,
            catalog_source: CatalogSource::Default,
            auth_manager,
        }
    }

    /// Configure a newly constructed manager to use only the provider's explicit catalog.
    /// Clear the bundled seed before sharing it; the existing auth identity must be retained.
    pub fn with_provider_catalog(mut self) -> Self {
        self.catalog_source = CatalogSource::ExplicitProvider(Mutex::new(()));
        self.remote_models.get_mut().models.clear();
        self
    }
}

impl StaticModelsManager {
    /// Construct a static model manager from an authoritative catalog.
    pub fn new(auth_manager: Option<Arc<AuthManager>>, model_catalog: ModelsResponse) -> Self {
        Self {
            remote_models: model_catalog.models,
            auth_manager,
        }
    }
}

impl ModelsManager for OpenAiModelsManager {
    fn set_api_key_model_discovery_enabled(&self, enabled: bool) {
        self.api_key_model_discovery_enabled
            .store(enabled, Ordering::SeqCst);
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(OpenAiModelsManager::raw_model_catalog(
            self,
            refresh_strategy,
            http_client_factory,
        ))
    }

    fn refresh_after_auth_change(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            let refresh = async {
                self.endpoint_client.reconcile_identity().await;
                // Resolve lazy command credentials before comparing catalog identities.
                if !self.should_refresh_models().await {
                    return Ok(());
                }
                let identity = self.endpoint_client.identity();
                if identity.is_some() && self.remote_models.read().await.identity == identity {
                    return Ok(());
                }
                self.refresh_available_models(
                    RefreshStrategy::OnlineIfUncached,
                    &http_client_factory,
                )
                .await
            };
            // Include auth resolution and cache access in the best-effort deadline.
            if !matches!(
                tokio::time::timeout(Duration::from_secs(/*secs*/ 5), refresh).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("model catalog refresh after auth change failed or timed out");
            }
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move {
            self.endpoint_client.reconcile_identity().await;
            let entry = self.remote_models.read().await;
            self.models_for_entry(&entry).into_owned()
        })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        if self.endpoint_client.requires_identity_reconciliation() {
            return Ok(Vec::new());
        }
        let entry = self.remote_models.try_read()?;
        Ok(self.models_for_entry(&entry).into_owned())
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn try_pinned_authority_model(
        &self,
        model: &str,
        config: &ModelsManagerConfig,
    ) -> Option<ModelInfo> {
        let identity = self.endpoint_client.try_pinned_identity()?;
        let entry = self.remote_models.try_read().ok()?;
        let rejected = self.rejected_identities.try_read().ok()?;
        if entry.identity.as_deref() != Some(identity.identity.as_str())
            || entry.fetched_at == chrono::DateTime::<Utc>::MIN_UTC
            || entry.client_version.as_deref() != Some(crate::client_version_to_whole().as_str())
            || entry
                .identity
                .as_ref()
                .is_some_and(|id| rejected.contains(id))
        {
            return None;
        }
        let discovery_disabled = identity.uses_api_key_auth
            && !self.endpoint_client.has_command_auth()
            && (!self.endpoint_client.supports_api_key_models()
                || !self.api_key_model_discovery_enabled.load(Ordering::SeqCst));
        // An authority snapshot may only describe the actual pinned entry. Normal
        // discovery can synthesize fallback metadata; that is not launch proof.
        if discovery_disabled
            && (!matches!(self.catalog_source, CatalogSource::Default)
                || self.catalog_source.fallback_models()?.as_slice() != entry.models.as_slice())
        {
            return None;
        }
        let catalog = entry.models.as_slice();
        let selected = construct_model_info_from_candidates(model, catalog, config);
        (!selected.used_fallback_model_metadata).then_some(selected)
    }

    #[tracing::instrument(skip_all, fields(model = model))]
    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(async move {
            self.endpoint_client.reconcile_identity().await;
            let remote = {
                let entry = self.remote_models.read().await;
                let models = self.models_for_entry(&entry);
                match &self.catalog_source {
                    CatalogSource::Default => find_model_from_candidates(model, &models),
                    CatalogSource::ExplicitProvider(_) => models
                        .iter()
                        .find(|candidate| candidate.slug == model)
                        .cloned(),
                }
            };
            construct_model_info(model, remote, config)
        })
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn refresh_if_new_etag(
        &self,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(OpenAiModelsManager::refresh_if_new_etag(
            self,
            etag,
            http_client_factory,
        ))
    }
}

impl OpenAiModelsManager {
    // Rejections survive account switches even if persistent invalidation failed.
    fn identity_rejected(&self, identity: &str) -> bool {
        self.rejected_identities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(identity)
    }

    /// Selects the catalog available under the current provider and auth identity.
    fn models_for_entry<'a>(&self, entry: &'a ModelsCacheEntry) -> Cow<'a, [ModelInfo]> {
        if entry
            .identity
            .as_ref()
            .is_some_and(|identity| self.identity_rejected(identity))
        {
            return Cow::Borrowed(&[]);
        }
        if self.endpoint_client.requires_identity_reconciliation()
            && self.endpoint_client.identity().is_none()
        {
            return Cow::Borrowed(&[]);
        }
        if !self.api_key_discovery_disabled()
            && entry.identity.is_some()
            && entry.identity == self.endpoint_client.identity()
        {
            Cow::Borrowed(&entry.models)
        } else {
            Cow::Owned(self.catalog_source.fallback_models().unwrap_or_default())
        }
    }

    async fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsResponse {
        if let Err(err) = self
            .refresh_available_models(refresh_strategy, &http_client_factory)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        ModelsResponse {
            models: self.get_remote_models().await,
        }
    }

    async fn refresh_if_new_etag(&self, etag: String, http_client_factory: HttpClientFactory) {
        let refresh_guard = self.catalog_source.refresh_lock().await;
        self.endpoint_client.reconcile_identity().await;
        let (identity, current_etag) = {
            let entry = self.remote_models.read().await;
            (entry.identity.clone(), entry.etag.clone())
        };
        if let Some(identity) = identity
            && Some(&identity) == self.endpoint_client.identity().as_ref()
            && current_etag.as_deref() == Some(etag.as_str())
        {
            if let Some(cache) = self.cache.as_ref()
                && let Err(err) = cache
                    .refresh_ttl(&crate::client_version_to_whole(), &identity, &etag)
                    .await
            {
                error!("failed to renew cache TTL: {err}");
            }
            return;
        }
        drop(refresh_guard);
        if let Err(err) = self
            .refresh_available_models(RefreshStrategy::Online, &http_client_factory)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: &HttpClientFactory,
    ) -> CoreResult<()> {
        let _refresh_guard = self.catalog_source.refresh_lock().await;
        self.endpoint_client.reconcile_identity().await;
        // API-key discovery must be enabled and supported before reusing a remote catalog.
        // Otherwise even a matching cache from an earlier run would bypass bundled-only behavior.
        // Command-auth providers retain their existing discovery behavior.
        if self.api_key_discovery_disabled() {
            return Ok(());
        }
        if !self.should_refresh_models().await {
            if matches!(
                refresh_strategy,
                RefreshStrategy::Offline | RefreshStrategy::OnlineIfUncached
            ) {
                self.try_load_cache().await;
            }
            return Ok(());
        }
        match refresh_strategy {
            RefreshStrategy::Offline => {
                self.try_load_cache().await;
                Ok(())
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache().await {
                    return Ok(());
                }
                self.fetch_and_update_models(http_client_factory).await
            }
            RefreshStrategy::Online => self.fetch_and_update_models(http_client_factory).await,
        }
    }

    async fn fetch_and_update_models(
        &self,
        http_client_factory: &HttpClientFactory,
    ) -> CoreResult<()> {
        let client_version = crate::client_version_to_whole();
        let ModelsEndpointResponse {
            models,
            etag,
            identity,
        } = match self
            .endpoint_client
            .list_models(&client_version, http_client_factory.clone())
            .await
        {
            Ok(response) => response,
            Err(ModelsEndpointError {
                error,
                identity: request_identity,
            }) => {
                // A failed fetch is not an authoritative empty catalog. Keep the last
                // successful snapshot (and its original freshness) unless access was
                // explicitly rejected. Provider/auth identity checks still govern reuse.
                let is_provider_catalog =
                    matches!(&self.catalog_source, CatalogSource::ExplicitProvider(_));
                let access_denied = matches!(error.http_status_code_value(), Some(401 | 403));
                let credentials_rejected = matches!(
                    error.details(),
                    CodexErrorDetails::RefreshTokenFailed(_)
                        | CodexErrorDetails::GatewayLoginRequired(_)
                );

                if is_provider_catalog && (access_denied || credentials_rejected) {
                    if let Some(identity) = request_identity.as_ref() {
                        self.rejected_identities
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(identity.clone());
                    }
                    self.endpoint_client.reconcile_identity().await;
                    {
                        let mut current = self.remote_models.write().await;
                        if request_identity.is_some() && current.identity == request_identity {
                            current.fetched_at = DateTime::<Utc>::MIN_UTC;
                            current.models.clear();
                            current.etag = None;
                        }
                    }
                    if let Some(identity) = request_identity
                        && let Some(cache) = self.cache.as_ref()
                    {
                        let invalidation = async {
                            if let Some(mut entry) =
                                cache.load_scoped(&client_version, &identity).await?
                                && entry.identity.as_deref() == Some(&identity)
                                && entry.client_version.as_deref() == Some(&client_version)
                            {
                                // Preserve backend lookup keys, but force a cache miss after restart.
                                entry.fetched_at = DateTime::<Utc>::MIN_UTC;
                                entry.etag = None;
                                entry.models.clear();
                                cache.store(&entry).await?;
                            }
                            Ok::<_, crate::cache::ModelsCacheError>(())
                        };
                        if let Err(err) = invalidation.await {
                            error!("failed to invalidate models cache: {err}");
                        }
                    }
                }
                return Err(error);
            }
        };
        self.rejected_identities
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&identity);
        self.endpoint_client.reconcile_identity().await;
        if Some(&identity) != self.endpoint_client.identity().as_ref() {
            return Ok(());
        }
        let entry = ModelsCacheEntry {
            fetched_at: Utc::now(),
            etag,
            client_version: Some(client_version),
            identity: Some(identity),
            models,
        };
        if let Some(cache) = self.cache.as_ref()
            && let Err(err) = cache.store(&entry).await
        {
            error!("failed to write models cache: {err}");
        }
        self.apply_remote_models(entry).await;
        Ok(())
    }

    fn supports_api_key_discovery(&self) -> bool {
        self.endpoint_client.supports_api_key_models()
            && !self.endpoint_client.has_command_auth()
            && self.uses_api_key_auth()
    }

    // A runtime opt-out must hide catalogs fetched before the flag was disabled, too.
    fn api_key_discovery_disabled(&self) -> bool {
        self.uses_api_key_auth()
            && !self.endpoint_client.has_command_auth()
            && (!self.endpoint_client.supports_api_key_models()
                || !self.api_key_model_discovery_enabled.load(Ordering::SeqCst))
    }

    fn uses_api_key_auth(&self) -> bool {
        self.endpoint_client.has_provider_api_key()
            || self
                .auth_manager
                .as_ref()
                .is_some_and(|auth_manager| auth_manager.auth_mode() == Some(AuthMode::ApiKey))
    }

    async fn should_refresh_models(&self) -> bool {
        self.endpoint_client.uses_codex_backend().await
            || self.endpoint_client.has_command_auth()
            || self.supports_api_key_discovery()
            || matches!(&self.catalog_source, CatalogSource::ExplicitProvider(_))
    }

    /// Publish only while the request identity still matches, including after async storage.
    async fn apply_remote_models(&self, mut entry: ModelsCacheEntry) -> bool {
        self.endpoint_client.reconcile_identity().await;
        let mut current = self.remote_models.write().await;
        if entry.identity != self.endpoint_client.identity()
            || entry
                .identity
                .as_ref()
                .is_some_and(|identity| self.identity_rejected(identity))
        {
            return false;
        }
        let remote_only = entry
            .models
            .iter()
            .any(|model| model.visibility == ModelVisibility::List)
            && (self.supports_api_key_discovery()
                || self.auth_manager.as_ref().is_some_and(|auth_manager| {
                    auth_manager
                        .auth_mode()
                        .is_some_and(AuthMode::has_chatgpt_account)
                }));
        if !remote_only && let Some(mut models) = self.catalog_source.fallback_models() {
            for model in entry.models {
                if let Some(index) = models
                    .iter()
                    .position(|existing| existing.slug == model.slug)
                {
                    models[index] = model;
                } else {
                    models.push(model);
                }
            }
            entry.models = models;
        }
        *current = entry;
        true
    }

    /// Attempt to satisfy the refresh from the cache when it matches the provider and TTL.
    async fn try_load_cache(&self) -> bool {
        let Some(cache) = self.cache.as_ref() else {
            return false;
        };
        self.endpoint_client.reconcile_identity().await;
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let client_version = crate::client_version_to_whole();
        info!(client_version, "models cache: evaluating cache eligibility");
        let Some(identity) = self.endpoint_client.identity() else {
            return false;
        };
        if self.identity_rejected(&identity) {
            return false;
        }
        {
            let current = self.remote_models.read().await;
            // An access rejection must not be replaced by stale cache data if invalidation failed.
            if current.fetched_at == DateTime::<Utc>::MIN_UTC
                && current.identity.as_ref() == Some(&identity)
            {
                return false;
            }
        }
        let cache_entry = match cache.load_scoped(&client_version, &identity).await {
            Ok(Some(cache_entry)) => cache_entry,
            Ok(None) => {
                info!("models cache: no usable cache entry");
                return false;
            }
            Err(err) => {
                error!("failed to load models cache: {err}");
                return false;
            }
        };
        if cache_entry.fetched_at == DateTime::<Utc>::MIN_UTC {
            return false;
        }
        if cache_entry.client_version.as_deref() != Some(client_version.as_str()) {
            info!(
                expected_version = client_version,
                cached_version = ?cache_entry.client_version,
                "models cache: cache version mismatch"
            );
            return false;
        }
        if cache_entry.identity.as_ref() != Some(&identity)
            || self.endpoint_client.identity().as_ref() != Some(&identity)
        {
            info!("models cache: provider or auth identity mismatch");
            return false;
        }
        self.apply_remote_models(cache_entry).await
    }
}

impl ModelsManager for StaticModelsManager {
    fn try_pinned_authority_model(
        &self,
        model: &str,
        config: &ModelsManagerConfig,
    ) -> Option<ModelInfo> {
        let selected = construct_model_info_from_candidates(model, &self.remote_models, config);
        (!selected.used_fallback_model_metadata).then_some(selected)
    }

    #[tracing::instrument(skip_all, fields(model = model))]
    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(
            async move { construct_model_info_from_candidates(model, &self.remote_models, config) },
        )
    }

    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        allow_provider_model_fallback: bool,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'a, String> {
        Box::pin(
            async move {
                let available_models = self
                    .list_models(refresh_strategy, http_client_factory)
                    .await;
                let requested_model = model.as_deref();

                if allow_provider_model_fallback {
                    if requested_model_is_available(requested_model, &available_models)
                        && let Some(requested_model) = requested_model
                    {
                        return requested_model.to_string();
                    }
                    return default_model_from_available(available_models);
                }

                model
                    .clone()
                    .unwrap_or_else(|| default_model_from_available(available_models))
            }
            .instrument(tracing::info_span!(
                "get_default_model",
                model.provided = model.is_some(),
                allow_provider_model_fallback,
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    fn raw_model_catalog(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(async move {
            ModelsResponse {
                models: self.get_remote_models().await,
            }
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async { self.remote_models.clone() })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self.remote_models.clone())
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn refresh_if_new_etag(
        &self,
        _etag: String,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async {})
    }
}

fn load_remote_models_from_file() -> Result<Vec<ModelInfo>, std::io::Error> {
    Ok(crate::bundled_models_response()?.models)
}

fn default_model_from_available(available: Vec<ModelPreset>) -> String {
    available
        .iter()
        .find(|model| model.is_default)
        .or_else(|| available.first())
        .map(|model| model.model.clone())
        .unwrap_or_default()
}

fn requested_model_is_available(
    requested_model: Option<&str>,
    available_models: &[ModelPreset],
) -> bool {
    requested_model.is_some_and(|requested_model| {
        available_models
            .iter()
            .any(|available_model| available_model.model == requested_model)
    })
}

fn find_model_by_longest_prefix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    let mut best: Option<ModelInfo> = None;
    for candidate in candidates {
        if !model.starts_with(&candidate.slug) {
            continue;
        }
        let is_better_match = if let Some(current) = best.as_ref() {
            candidate.slug.len() > current.slug.len()
        } else {
            true
        };
        if is_better_match {
            best = Some(candidate.clone());
        }
    }
    best
}

fn find_model_by_namespaced_suffix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    // Retry metadata lookup for a single namespaced slug like `namespace/model-name`.
    //
    // This only strips one leading namespace segment and only when the namespace looks
    // like a simple provider id to avoid broadly matching arbitrary aliases.
    let (namespace, suffix) = model.split_once('/')?;
    if suffix.contains('/') {
        return None;
    }
    if namespace.is_empty()
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    find_model_by_longest_prefix(suffix, candidates)
}

/// Selects owned model metadata without applying configuration overrides.
fn find_model_from_candidates(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    // First use the normal longest-prefix match. If that misses, allow a narrowly scoped
    // retry for namespaced slugs like `custom/gpt-5.3-codex`.
    find_model_by_longest_prefix(model, candidates)
        .or_else(|| find_model_by_namespaced_suffix(model, candidates))
}

pub(crate) fn construct_model_info_from_candidates(
    model: &str,
    candidates: &[ModelInfo],
    config: &ModelsManagerConfig,
) -> ModelInfo {
    let remote = find_model_from_candidates(model, candidates);
    construct_model_info(model, remote, config)
}

fn construct_model_info(
    model: &str,
    remote: Option<ModelInfo>,
    config: &ModelsManagerConfig,
) -> ModelInfo {
    let model_info = if let Some(remote) = remote {
        ModelInfo {
            slug: model.to_string(),
            used_fallback_model_metadata: false,
            ..remote
        }
    } else {
        model_info::model_info_from_slug(model)
    };
    model_info::with_config_overrides(model_info, config)
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
