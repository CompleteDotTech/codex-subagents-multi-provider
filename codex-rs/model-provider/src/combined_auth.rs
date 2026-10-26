//! Composes primary provider credentials with independently managed gateway OAuth credentials.

use std::sync::Arc;

use crate::auth::ResolvedProviderAuth;
use codex_api::AuthError;
use codex_api::AuthHeadersFuture;
use codex_api::AuthProvider;
use codex_api::AuthProviderFuture;
use codex_api::SharedAuthProvider;
use codex_login::GatewayAuthError;
use codex_login::GatewayAuthManager;
use codex_model_provider_info::GatewayOAuthConfig;
use codex_model_provider_info::GatewayOAuthDelivery;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;

pub(crate) async fn compose_auth(
    provider: &ModelProviderInfo,
    manager: Option<&std::result::Result<Arc<GatewayAuthManager>, String>>,
    resolved: ResolvedProviderAuth,
) -> Result<ResolvedProviderAuth> {
    if provider.gateway_oauth.is_none() {
        return Ok(resolved);
    }
    provider.validate().map_err(CodexErr::InvalidRequest)?;
    let manager = manager.ok_or_else(|| {
        CodexErr::InvalidRequest("gateway_oauth requires auth runtime configuration".into())
    })?;
    let manager = manager
        .as_ref()
        .map_err(|error| CodexErr::InvalidRequest(error.clone()))?;
    let token = manager
        .resolve_access_token()
        .await
        .map_err(gateway_auth_error)?;
    compose_auth_with_token(provider, &token, resolved)
}

pub(crate) async fn compose_catalog_auth(
    provider: &ModelProviderInfo,
    manager: Option<&std::result::Result<Arc<GatewayAuthManager>, String>>,
    resolved: ResolvedProviderAuth,
) -> Result<(ResolvedProviderAuth, Option<String>)> {
    if provider.gateway_oauth.is_none() {
        return Ok((resolved, None));
    }
    provider.validate().map_err(CodexErr::InvalidRequest)?;
    let manager = manager
        .ok_or_else(|| {
            CodexErr::InvalidRequest("gateway_oauth requires auth runtime configuration".into())
        })?
        .as_ref()
        .map_err(|error| CodexErr::InvalidRequest(error.clone()))?;
    let credentials = manager
        .begin_request()
        .credentials()
        .await
        .map_err(gateway_auth_error)?;
    let identity = credentials.cache_identity(&manager.credential_id());
    Ok((
        compose_auth_with_token(provider, &credentials.access_token, resolved)?,
        Some(identity),
    ))
}

fn gateway_auth_error(error: std::io::Error) -> CodexErr {
    if let Some(login_error) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<GatewayAuthError>())
    {
        return match login_error {
            GatewayAuthError::LoginRequired => {
                CodexErrorDetails::GatewayLoginRequired(login_error.to_string()).into()
            }
            GatewayAuthError::LoginInProgress => CodexErr::InvalidRequest(login_error.to_string()),
        };
    }
    // Issuer errors may echo arbitrary credentials from configured URLs. Keep the
    // diagnostic safe and bounded, preserving retries for transient failures.
    CodexErr::Io(std::io::Error::new(
        error.kind(),
        "Gateway OAuth authentication failed; check the gateway configuration and credential store.",
    ))
}

/// Compose a credential already captured for this request, without resolving a
/// second token that could belong to a different authorization grant.
pub(crate) fn compose_auth_with_token(
    provider: &ModelProviderInfo,
    token: &str,
    mut resolved: ResolvedProviderAuth,
) -> Result<ResolvedProviderAuth> {
    let Some(config) = provider.gateway_oauth.as_ref() else {
        return Ok(resolved);
    };
    provider.validate().map_err(CodexErr::InvalidRequest)?;
    let (name, value) = gateway_header(config, token)?;
    if resolved
        .auth
        .to_auth_headers()
        .map_err(|error| codex_api::map_api_error(error.into()))?
        .contains_key(&name)
    {
        return Err(CodexErr::InvalidRequest(
            "gateway OAuth conflicts with primary auth headers".into(),
        ));
    }
    resolved.auth = Arc::new(CombinedAuth {
        primary: resolved.auth,
        name,
        value,
    });
    Ok(resolved)
}

fn gateway_header(config: &GatewayOAuthConfig, token: &str) -> Result<(HeaderName, HeaderValue)> {
    if token.is_empty() {
        return Err(CodexErr::InvalidRequest(
            "gateway OAuth returned an empty token".into(),
        ));
    }
    let (name, value) = match &config.delivery {
        GatewayOAuthDelivery::Header { name, scheme } => (
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| CodexErr::InvalidRequest("invalid gateway header".into()))?,
            format!("{scheme} {token}"),
        ),
        GatewayOAuthDelivery::Cookie { name } => {
            // RFC 6265 cookie-octet excludes separators, quotes, whitespace and non-ASCII.
            if !token.bytes().all(
                |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
            ) {
                return Err(CodexErr::InvalidRequest(
                    "gateway OAuth token is not a valid cookie value".into(),
                ));
            }
            (http::header::COOKIE, format!("{name}={token}"))
        }
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| CodexErr::InvalidRequest("invalid gateway OAuth token header".into()))?;
    value.set_sensitive(true);
    Ok((name, value))
}

struct CombinedAuth {
    primary: SharedAuthProvider,
    name: HeaderName,
    value: HeaderValue,
}

impl AuthProvider for CombinedAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> std::result::Result<(), AuthError> {
        self.primary.add_auth_headers(headers)?;
        headers
            .entry(self.name.clone())
            .or_insert(self.value.clone());
        Ok(())
    }

    fn resolve_auth_headers(&self) -> AuthHeadersFuture<'_> {
        Box::pin(async move {
            let mut headers = self.primary.resolve_auth_headers().await?;
            if headers.contains_key(&self.name) {
                return Err(AuthError::Build(
                    "gateway OAuth conflicts with primary auth headers".into(),
                ));
            }
            headers.insert(self.name.clone(), self.value.clone());
            Ok(headers)
        })
    }

    fn apply_auth(&self, request: codex_http_client::Request) -> AuthProviderFuture<'_> {
        Box::pin(async move {
            let mut request = self.primary.apply_auth(request).await?;
            if request.headers.contains_key(&self.name) {
                return Err(AuthError::Build(
                    "gateway OAuth conflicts with request headers".into(),
                ));
            }
            request
                .headers
                .insert(self.name.clone(), self.value.clone());
            Ok(request)
        })
    }
}
