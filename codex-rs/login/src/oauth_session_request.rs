//! Coherent request credentials and unauthorized recovery for configured OAuth consumers.
//! The session manager owns observation, refresh, locking, and persistence; a request
//! retains only its rejected token so transport retries cannot forget recovery.

use super::OAuthSessionManager;
use super::RefreshPolicy;
use super::login;
use sha2::Digest;
use sha2::Sha256;
use std::fmt;
use std::io;

/// A token and the identity of the exact grant that supplied it.
pub struct OAuthRequestCredentials {
    pub access_token: String,
    pub authorization_id: Option<String>,
}

impl OAuthRequestCredentials {
    /// Opaque scope for the exact credential pair used by a request. Token
    /// rotation invalidates derived caches, including grants without an ID.
    pub fn cache_identity(&self, credential_id: &str) -> String {
        credential_cache_identity(
            credential_id,
            &self.access_token,
            self.authorization_id.as_deref(),
        )
    }
}

pub(super) fn credential_cache_identity(
    credential_id: &str,
    token: &str,
    authorization_id: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update([u8::from(authorization_id.is_some())]);
    for field in [
        "oauth-derived-cache-v1",
        credential_id,
        token,
        authorization_id.unwrap_or_default(),
    ] {
        digest.update((field.len() as u64).to_le_bytes());
        digest.update(field.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

impl fmt::Debug for OAuthRequestCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthRequestCredentials")
            .field("access_token", &"<redacted>")
            .field("authorization_id", &self.authorization_id)
            .finish()
    }
}

/// One consumer request and its retries, using the existing session's credential state.
pub struct OAuthSessionRequest {
    manager: OAuthSessionManager,
    policy: RefreshPolicy,
    sent_token: Option<String>,
}

impl OAuthSessionRequest {
    pub(super) fn new(manager: OAuthSessionManager) -> Self {
        Self {
            manager,
            policy: RefreshPolicy::ObserveStored,
            sent_token: None,
        }
    }

    /// Observes persisted credentials using the existing session lifecycle. The returned
    /// identity is captured with the token, never read separately after resolution.
    pub async fn credentials(&mut self) -> io::Result<OAuthRequestCredentials> {
        let token = self
            .manager
            .resolve_credentials_with_browser(self.policy.clone(), login::open_browser)
            .await?;
        self.sent_token = Some(token.access_token.clone());
        Ok(OAuthRequestCredentials {
            access_token: token.access_token,
            authorization_id: token.authorization_id,
        })
    }

    /// Marks the sent credential as rejected. Callers bound unauthorized replays;
    /// transient transport failures retain this state until resolution succeeds.
    pub fn reject_credentials(&mut self) -> bool {
        let Some(token) = self.sent_token.take() else {
            return false;
        };
        self.policy = RefreshPolicy::AfterRejection(token);
        true
    }

    /// Identifies credential failures that belong to unauthorized recovery.
    pub fn is_recovering(&self) -> bool {
        matches!(self.policy, RefreshPolicy::AfterRejection(_))
    }
}
