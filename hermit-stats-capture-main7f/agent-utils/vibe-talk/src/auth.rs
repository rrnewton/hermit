//! Who is allowed to call this server, and for what.
//!
//! The server is intended to become publicly reachable, and it holds a bot token that can post to
//! the owner's channels. Two rules follow, and they are the whole of v0's access control:
//!
//! * every `/api/` route requires a bearer token — there is no unauthenticated read; and
//! * posting requires a *different, stronger* token than reading, so a credential handed to a
//!   summarizing agent cannot be turned into a way to speak as the owner.
//!
//! This is coarse. It is deliberately coarse: a small rule that is actually enforced beats a
//! permission model that is described in a README. See the threat model in the project README for
//! what this does NOT cover.

use crate::config::AuthConfig;

/// What a caller is permitted to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Read channel history and summaries.
    Read,
    /// Read, and post messages to a writable channel.
    Write,
}

/// Why a request was not allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No usable credential was presented.
    #[error("missing or malformed bearer token")]
    Unauthenticated,
    /// A valid credential without sufficient scope.
    #[error("this token may read but not post")]
    Forbidden,
}

/// Extract the token from an `Authorization` header value.
///
/// The scheme match is case-insensitive because HTTP says so; the token itself is not touched.
#[must_use]
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token)
}

/// Determine the scope a presented credential carries.
#[must_use]
pub fn scope_of(header: Option<&str>, config: &AuthConfig) -> Option<Scope> {
    let token = bearer_token(header)?;
    // Check write first: the two tokens are required to differ, so this cannot mask a read token.
    if config.write_token.matches(token) {
        return Some(Scope::Write);
    }
    if config.read_token.matches(token) {
        return Some(Scope::Read);
    }
    None
}

/// Authorize a request that requires `required`.
///
/// # Errors
///
/// Returns [`AuthError::Unauthenticated`] when no valid token was presented, and
/// [`AuthError::Forbidden`] when a valid token lacks the required scope.
pub fn authorize(
    header: Option<&str>,
    config: &AuthConfig,
    required: Scope,
) -> Result<Scope, AuthError> {
    let scope = scope_of(header, config).ok_or(AuthError::Unauthenticated)?;
    if scope < required {
        return Err(AuthError::Forbidden);
    }
    Ok(scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    fn config() -> AuthConfig {
        AuthConfig {
            read_token: Secret::new("read-token-that-is-long-enough"),
            write_token: Secret::new("write-token-that-is-long-enough"),
        }
    }

    #[test]
    fn a_missing_header_is_unauthenticated() {
        assert_eq!(
            authorize(None, &config(), Scope::Read),
            Err(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn a_wrong_token_is_unauthenticated() {
        assert_eq!(
            authorize(
                Some("Bearer nope-nope-nope-nope-nope"),
                &config(),
                Scope::Read
            ),
            Err(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn a_non_bearer_scheme_is_rejected() {
        assert_eq!(
            authorize(
                Some("Basic read-token-that-is-long-enough"),
                &config(),
                Scope::Read
            ),
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(
            authorize(
                Some("read-token-that-is-long-enough"),
                &config(),
                Scope::Read
            ),
            Err(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(
            authorize(
                Some("bEaReR read-token-that-is-long-enough"),
                &config(),
                Scope::Read
            ),
            Ok(Scope::Read)
        );
    }

    #[test]
    fn the_read_token_cannot_post() {
        assert_eq!(
            authorize(
                Some("Bearer read-token-that-is-long-enough"),
                &config(),
                Scope::Write
            ),
            Err(AuthError::Forbidden)
        );
    }

    #[test]
    fn the_write_token_can_also_read() {
        assert_eq!(
            authorize(
                Some("Bearer write-token-that-is-long-enough"),
                &config(),
                Scope::Read
            ),
            Ok(Scope::Write)
        );
        assert_eq!(
            authorize(
                Some("Bearer write-token-that-is-long-enough"),
                &config(),
                Scope::Write
            ),
            Ok(Scope::Write)
        );
    }

    #[test]
    fn an_empty_bearer_value_is_rejected() {
        assert_eq!(bearer_token(Some("Bearer    ")), None);
        assert_eq!(bearer_token(Some("Bearer")), None);
    }

    #[test]
    fn a_token_with_surrounding_whitespace_still_matches() {
        assert_eq!(
            authorize(
                Some("  Bearer read-token-that-is-long-enough  "),
                &config(),
                Scope::Read
            ),
            Ok(Scope::Read)
        );
    }
}
