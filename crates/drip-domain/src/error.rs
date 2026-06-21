//! The domain-level error type. Infrastructure errors (HTTP, sqlite, keychain) map into
//! these variants at the boundary, so inner layers never see technology-specific errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("market data unavailable: {0}")]
    MarketData(String),

    #[error("strategy error: {0}")]
    Strategy(String),

    #[error("broker error: {0}")]
    Broker(String),

    #[error("capability not supported by this broker: {0}")]
    Unsupported(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("secret store error: {0}")]
    Secret(String),

    #[error("configuration error: {0}")]
    Config(String),
}

/// Convenience alias used throughout the domain ports.
pub type Result<T> = std::result::Result<T, DomainError>;
