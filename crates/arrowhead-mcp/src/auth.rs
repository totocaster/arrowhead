//! Authentication and network policy primitives for the MCP HTTP server.

use std::{
    fmt, fs,
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use hex::FromHexError;
use ipnet::IpNet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Supported authentication modes for the MCP HTTP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Require `Authorization: Bearer <token>` headers.
    #[default]
    Bearer,
    /// Accept link tokens embedded in the request path (`/rpc/<token>`).
    LinkToken,
}

impl AuthMode {
    /// Determine whether link tokens embedded in the URL path are accepted.
    #[must_use]
    pub const fn accepts_link_tokens(self) -> bool {
        matches!(self, Self::LinkToken)
    }
}

impl fmt::Display for AuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            AuthMode::Bearer => "bearer",
            AuthMode::LinkToken => "link-token",
        };
        f.write_str(label)
    }
}

impl FromStr for AuthMode {
    type Err = AuthModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bearer" => Ok(AuthMode::Bearer),
            "link-token" | "link_token" => Ok(AuthMode::LinkToken),
            other => Err(AuthModeParseError {
                provided: other.to_string(),
            }),
        }
    }
}

/// Error returned when parsing an [`AuthMode`] from user input fails.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid authentication mode '{provided}'")]
pub struct AuthModeParseError {
    provided: String,
}

/// Error conditions that can occur while configuring token-based authentication.
#[derive(Debug, Error)]
pub enum TokenError {
    /// Caller attempted to add an empty token string.
    #[error("authentication token value cannot be empty")]
    EmptyToken,
    /// Provided hexadecimal digest was invalid.
    #[error("invalid token digest '{input}': {source}")]
    InvalidDigest {
        /// Original user-provided representation.
        input: String,
        /// Underlying parse failure.
        #[source]
        source: FromHexError,
    },
    /// Reading a token file from disk failed.
    #[error("failed to read token file {path}: {source}")]
    TokenFileRead {
        /// Path of the file that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Token store was empty after applying all sources.
    #[error("token store does not contain any entries")]
    EmptyStore,
}

/// Representation of a hashed authentication token (SHA-256 digests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Hash a raw token string using SHA-256.
    #[must_use]
    pub fn hash(raw: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    /// Construct a digest from a hexadecimal representation (optionally prefixed by `sha256:`).
    pub fn from_hex(input: &str) -> Result<Self, FromHexError> {
        let trimmed = input.trim();
        let value = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(value, &mut bytes)?;
        Ok(Self(bytes))
    }

    /// Expose the digest as a lowercase hexadecimal string.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    fn ct_eq(&self, other: &TokenDigest) -> subtle::Choice {
        self.0.ct_eq(&other.0)
    }
}

impl Serialize for TokenDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for TokenDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        TokenDigest::from_hex(&value).map_err(|err| {
            serde::de::Error::custom(format!("invalid token digest '{value}': {err}"))
        })
    }
}

impl fmt::Display for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Collection of hashed tokens available for authentication.
#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    digests: Arc<[TokenDigest]>,
}

impl TokenStore {
    /// Construct a token store from pre-computed digests.
    #[must_use]
    pub fn new(digests: Vec<TokenDigest>) -> Self {
        Self {
            digests: Arc::from(digests.into_boxed_slice()),
        }
    }

    /// Number of digests tracked by the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// Iterate over stored digests.
    pub fn digests(&self) -> &[TokenDigest] {
        &self.digests
    }

    /// Verify whether the supplied raw token matches any stored digest.
    #[must_use]
    pub fn verify(&self, token: &str) -> bool {
        if self.digests.is_empty() || token.is_empty() {
            return false;
        }

        let candidate = TokenDigest::hash(token);
        let mut matches = subtle::Choice::from(0);
        for digest in self.digests.iter() {
            matches |= digest.ct_eq(&candidate);
        }
        matches.unwrap_u8() == 1
    }
}

/// Builder used to compose a [`TokenStore`] from multiple sources.
#[derive(Debug, Default)]
pub struct TokenStoreBuilder {
    digests: Vec<TokenDigest>,
}

impl TokenStoreBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a raw (plaintext) token, hashing it before storage.
    pub fn add_raw_token(&mut self, token: impl AsRef<str>) -> Result<(), TokenError> {
        let token = token.as_ref().trim();
        if token.is_empty() {
            return Err(TokenError::EmptyToken);
        }
        self.digests.push(TokenDigest::hash(token));
        Ok(())
    }

    /// Add a pre-hashed token digest.
    pub fn add_hashed_token(&mut self, digest: TokenDigest) {
        self.digests.push(digest);
    }

    /// Parse and add a hashed token expressed as hexadecimal.
    pub fn add_hashed_hex(&mut self, value: impl AsRef<str>) -> Result<(), TokenError> {
        let value = value.as_ref();
        match TokenDigest::from_hex(value) {
            Ok(digest) => {
                self.add_hashed_token(digest);
                Ok(())
            }
            Err(source) => Err(TokenError::InvalidDigest {
                input: value.trim().to_string(),
                source,
            }),
        }
    }

    /// Load tokens from the supplied file.
    pub fn add_tokens_from_file(
        &mut self,
        path: impl AsRef<Path>,
        format: TokenFormat,
    ) -> Result<(), TokenError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).map_err(|source| TokenError::TokenFileRead {
            path: path.to_path_buf(),
            source,
        })?;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            match format {
                TokenFormat::Raw => self.add_raw_token(trimmed)?,
                TokenFormat::Hashed => self.add_hashed_hex(trimmed)?,
            }
        }

        Ok(())
    }

    /// Finalise the builder into an immutable store.
    #[must_use]
    pub fn build(self) -> TokenStore {
        TokenStore::new(self.digests)
    }
}

/// Token representation formats used when loading external sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenFormat {
    /// Tokens are stored in plaintext and must be hashed before use.
    Raw,
    /// Tokens are stored as hexadecimal SHA-256 digests (with optional `sha256:` prefix).
    Hashed,
}

/// Describes the set of sources that should be consulted when composing the
/// authentication token store.
#[derive(Debug, Default)]
pub struct TokenSources {
    raw_tokens: Vec<String>,
    hashed_tokens: Vec<TokenDigest>,
    token_files: Vec<TokenFileSource>,
    env_token: Option<String>,
}

impl TokenSources {
    /// Create an empty collection of token sources.
    pub fn new() -> Self {
        Self::default()
    }

    /// Include a raw token value.
    pub fn add_raw_token(&mut self, token: impl Into<String>) {
        self.raw_tokens.push(token.into());
    }

    /// Include a pre-hashed token digest.
    pub fn add_hashed_token(&mut self, digest: TokenDigest) {
        self.hashed_tokens.push(digest);
    }

    /// Parse and include a hashed token expressed as hexadecimal.
    pub fn add_hashed_token_hex(&mut self, value: impl AsRef<str>) -> Result<(), TokenError> {
        let digest =
            TokenDigest::from_hex(value.as_ref()).map_err(|source| TokenError::InvalidDigest {
                input: value.as_ref().trim().to_string(),
                source,
            })?;
        self.add_hashed_token(digest);
        Ok(())
    }

    /// Include tokens from a file located on disk.
    pub fn add_token_file(&mut self, path: impl Into<PathBuf>, format: TokenFormat) {
        self.token_files.push(TokenFileSource {
            path: path.into(),
            format,
        });
    }

    /// Provide an environment-sourced token (typically from `ARROWHEAD_MCP_TOKEN`).
    pub fn set_env_token(&mut self, token: Option<String>) {
        self.env_token = token;
    }

    /// Aggregate all configured sources into a [`TokenStore`].
    pub fn load(&self) -> Result<TokenStore, TokenError> {
        let mut builder = TokenStoreBuilder::new();

        if let Some(token) = self.env_token.as_deref() {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                builder.add_raw_token(trimmed)?;
            }
        }

        for token in &self.raw_tokens {
            builder.add_raw_token(token)?;
        }

        for digest in &self.hashed_tokens {
            builder.add_hashed_token(*digest);
        }

        for file in &self.token_files {
            builder.add_tokens_from_file(&file.path, file.format)?;
        }

        let store = builder.build();
        if store.is_empty() {
            Err(TokenError::EmptyStore)
        } else {
            Ok(store)
        }
    }
}

#[derive(Debug, Clone)]
struct TokenFileSource {
    path: PathBuf,
    format: TokenFormat,
}

/// Authentication configuration embedded in the HTTP server settings.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Selected authentication mode.
    pub mode: AuthMode,
    /// Hashed tokens accepted by the server.
    pub tokens: TokenStore,
}

impl AuthConfig {
    /// Create a new configuration using the supplied mode and token store.
    #[must_use]
    pub fn new(mode: AuthMode, tokens: TokenStore) -> Self {
        Self { mode, tokens }
    }

    /// Build an authenticator that can be shared across requests.
    pub fn build(&self) -> Result<Authenticator, TokenError> {
        Authenticator::new(self.mode, self.tokens.clone())
    }
}

/// Successful authentication metadata indicating the credential source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    /// Token presented via the `Authorization: Bearer` header.
    BearerHeader,
    /// Token embedded in the request path (link-token mode).
    LinkPath,
}

/// Failure reasons produced by the authenticator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// No credentials were presented by the client.
    MissingToken,
    /// Credentials were present but rejected.
    InvalidToken,
    /// Authentication could not proceed because the server lacks configured tokens.
    NotConfigured,
}

/// Performs per-request authentication checks.
#[derive(Debug, Clone)]
pub struct Authenticator {
    mode: AuthMode,
    tokens: TokenStore,
}

impl Authenticator {
    /// Construct a new authenticator.
    pub fn new(mode: AuthMode, tokens: TokenStore) -> Result<Self, TokenError> {
        if tokens.is_empty() {
            return Err(TokenError::EmptyStore);
        }
        Ok(Self { mode, tokens })
    }

    /// Access the authentication mode.
    #[must_use]
    pub const fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Authenticate an incoming request.
    pub fn authenticate(
        &self,
        bearer_header: Option<&str>,
        link_token: Option<&str>,
    ) -> Result<AuthSource, AuthFailure> {
        if self.tokens.is_empty() {
            return Err(AuthFailure::NotConfigured);
        }

        match self.mode {
            AuthMode::Bearer => {
                let token = match bearer_header.and_then(extract_bearer_token) {
                    Some(token) => token,
                    None => return Err(AuthFailure::MissingToken),
                };
                if self.tokens.verify(token) {
                    Ok(AuthSource::BearerHeader)
                } else {
                    Err(AuthFailure::InvalidToken)
                }
            }
            AuthMode::LinkToken => {
                if let Some(token) = link_token.and_then(non_empty_str) {
                    if self.tokens.verify(token) {
                        return Ok(AuthSource::LinkPath);
                    }
                }

                if let Some(token) = bearer_header.and_then(extract_bearer_token) {
                    if self.tokens.verify(token) {
                        return Ok(AuthSource::BearerHeader);
                    }
                    return Err(AuthFailure::InvalidToken);
                }

                if link_token.is_some() {
                    Err(AuthFailure::InvalidToken)
                } else {
                    Err(AuthFailure::MissingToken)
                }
            }
        }
    }
}

fn extract_bearer_token(header: &str) -> Option<&str> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(2, ' ');
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return None;
    }
    Some(token)
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Describes which IP addresses are allowed to communicate with the HTTP server.
#[derive(Debug, Clone)]
pub struct IpAllowList {
    entries: Arc<[IpNet]>,
}

impl Default for IpAllowList {
    fn default() -> Self {
        Self::localhost_only()
    }
}

impl IpAllowList {
    /// Create an allowlist that only permits loopback addresses by default.
    #[must_use]
    pub fn localhost_only() -> Self {
        Self::new(vec![
            "127.0.0.0/8".parse().expect("valid localhost IPv4 cidr"),
            "::1/128".parse().expect("valid localhost IPv6 cidr"),
        ])
    }

    /// Construct an allowlist from the supplied CIDR entries.
    #[must_use]
    pub fn new(entries: Vec<IpNet>) -> Self {
        Self {
            entries: Arc::from(entries.into_boxed_slice()),
        }
    }

    /// Determine whether the provided address is permitted.
    #[must_use]
    pub fn allows(&self, addr: IpAddr) -> bool {
        if self.entries.is_empty() {
            return true;
        }
        self.entries.iter().any(|entry| entry.contains(&addr))
    }

    /// Access the configured CIDR ranges.
    pub fn iter(&self) -> impl Iterator<Item = &IpNet> {
        self.entries.iter()
    }

    /// Compose an allowlist from user-provided CIDR strings.
    pub fn from_strings<I, S>(entries: I) -> Result<Self, NetworkError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut nets = Vec::new();
        for value in entries {
            let raw = value.as_ref().trim();
            if raw.is_empty() {
                continue;
            }
            match raw.parse::<IpNet>() {
                Ok(net) => nets.push(net),
                Err(source) => {
                    return Err(NetworkError::InvalidCidr {
                        input: raw.to_string(),
                        source,
                    });
                }
            }
        }

        Ok(Self::new(nets))
    }
}

/// Errors produced when parsing IP allowlist entries.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// Provided CIDR entry was malformed.
    #[error("invalid CIDR entry '{input}': {source}")]
    InvalidCidr {
        /// Original user-provided value.
        input: String,
        /// Underlying parse error from `ipnet`.
        #[source]
        source: ipnet::AddrParseError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn token_store_with(values: &[&str]) -> TokenStore {
        let mut builder = TokenStoreBuilder::new();
        for value in values {
            builder.add_raw_token(value).unwrap();
        }
        builder.build()
    }

    #[test]
    fn token_store_verifies_matches() {
        let store = token_store_with(&["alpha", "bravo"]);
        assert!(store.verify("alpha"));
        assert!(store.verify("bravo"));
        assert!(!store.verify("charlie"));
    }

    #[test]
    fn token_sources_loads_multiple_inputs() {
        let mut sources = TokenSources::new();
        sources.add_raw_token("one");
        sources.add_hashed_token(TokenDigest::hash("two"));
        sources.set_env_token(Some("three".to_string()));

        let store = sources.load().unwrap();
        assert_eq!(store.len(), 3);
        assert!(store.verify("one"));
        assert!(store.verify("two"));
        assert!(store.verify("three"));
    }

    #[test]
    fn token_sources_empty_is_error() {
        let sources = TokenSources::new();
        let err = sources.load().unwrap_err();
        matches!(err, TokenError::EmptyStore);
    }

    #[test]
    fn authenticator_supports_link_token_mode() {
        let tokens = token_store_with(&["secret"]);
        let auth = Authenticator::new(AuthMode::LinkToken, tokens).unwrap();
        assert_eq!(
            auth.authenticate(None, Some("secret")),
            Ok(AuthSource::LinkPath)
        );
        assert_eq!(
            auth.authenticate(Some("Bearer secret"), None),
            Ok(AuthSource::BearerHeader)
        );
        assert_eq!(
            auth.authenticate(Some("Bearer nope"), Some("wrong")),
            Err(AuthFailure::InvalidToken)
        );
        assert_eq!(
            auth.authenticate(None, None),
            Err(AuthFailure::MissingToken)
        );
    }

    #[test]
    fn authenticator_requires_bearer_header_in_default_mode() {
        let tokens = token_store_with(&["alpha"]);
        let auth = Authenticator::new(AuthMode::Bearer, tokens).unwrap();
        assert_eq!(
            auth.authenticate(Some("Bearer alpha"), None),
            Ok(AuthSource::BearerHeader)
        );
        assert_eq!(
            auth.authenticate(None, Some("alpha")),
            Err(AuthFailure::MissingToken)
        );
        assert_eq!(
            auth.authenticate(Some("Bearer wrong"), None),
            Err(AuthFailure::InvalidToken)
        );
    }

    #[test]
    fn ip_allow_list_defaults_to_localhost() {
        let list = IpAllowList::default();
        assert!(list.allows(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(list.allows(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!list.allows(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn ip_allow_list_from_strings() {
        let list = IpAllowList::from_strings(["10.0.0.0/8", "::/0"]).unwrap();
        assert!(list.allows(IpAddr::V4(Ipv4Addr::new(10, 5, 1, 2))));
        assert!(list.allows(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!list.allows(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))));
    }

    #[test]
    fn ip_allow_list_rejects_invalid_cidr() {
        let err = IpAllowList::from_strings(["abc"]).unwrap_err();
        match err {
            NetworkError::InvalidCidr { input, .. } => assert_eq!(input, "abc"),
        }
    }
}
