//! Reddit API credential storage: an OS-keyring-first layer with a
//! `DRIP_REDDIT_CLIENT_ID` / `DRIP_REDDIT_CLIENT_SECRET` environment
//! variable fallback for machines with no keyring backend available (e.g. a
//! headless Linux box).
//!
//! Two separate keyring entries are used (rather than serializing a struct
//! into one entry) so a partial failure (e.g. only one write succeeds) is
//! simple to reason about and each value can be read/written independently.

use anyhow::{anyhow, Result};
use keyring::Entry;

const SERVICE: &str = "drip";
const CLIENT_ID_ACCOUNT: &str = "reddit_client_id";
const CLIENT_SECRET_ACCOUNT: &str = "reddit_client_secret";

/// Save both credentials to the OS keyring. On any failure (most commonly:
/// no Secret Service / keychain backend available on this machine), returns
/// a clear error explaining that OS keyring storage isn't available and
/// that `DRIP_REDDIT_CLIENT_ID` / `DRIP_REDDIT_CLIENT_SECRET` environment
/// variables are the fallback -- never panics, never swallows the failure.
pub fn save_credentials(client_id: &str, client_secret: &str) -> Result<()> {
    save_one(CLIENT_ID_ACCOUNT, client_id)?;
    save_one(CLIENT_SECRET_ACCOUNT, client_secret)?;
    Ok(())
}

fn save_one(account: &str, value: &str) -> Result<()> {
    let entry = Entry::new(SERVICE, account).map_err(|err| keyring_unavailable_error(err))?;
    entry
        .set_password(value)
        .map_err(|err| keyring_unavailable_error(err))
}

fn keyring_unavailable_error(err: keyring::Error) -> anyhow::Error {
    anyhow!(
        "could not save credentials to the OS keyring ({err}). OS keyring storage isn't \
         available on this system. Set the DRIP_REDDIT_CLIENT_ID and DRIP_REDDIT_CLIENT_SECRET \
         environment variables instead."
    )
}

/// Load Reddit credentials, preferring the OS keyring and falling back to
/// environment variables.
///
/// Priority order:
/// 1. Both OS keyring entries present -> use them.
/// 2. Otherwise, both `DRIP_REDDIT_CLIENT_ID` / `DRIP_REDDIT_CLIENT_SECRET`
///    environment variables present -> use them.
/// 3. Otherwise, a clear error describing both ways to configure
///    credentials.
pub fn load_credentials() -> Result<(String, String)> {
    if let Some(pair) = load_from_keyring() {
        return Ok(pair);
    }

    if let Some(pair) = load_from_env() {
        return Ok(pair);
    }

    Err(anyhow!(
        "no Reddit API credentials configured. Configure them one of two ways:\n  \
         1. Run `drip init` to save them in your OS keyring.\n  \
         2. Export DRIP_REDDIT_CLIENT_ID and DRIP_REDDIT_CLIENT_SECRET as environment variables.\n\
         If you don't have a Reddit client id/secret yet, create a \"script\" app at \
         https://www.reddit.com/prefs/apps to get them."
    ))
}

/// Read both keyring entries. Any failure to reach the keyring backend
/// (e.g. no Secret Service provider installed) is treated as "not present"
/// rather than propagated -- the caller falls through to env vars.
fn load_from_keyring() -> Option<(String, String)> {
    let client_id = Entry::new(SERVICE, CLIENT_ID_ACCOUNT)
        .ok()
        .and_then(|entry| entry.get_password().ok());
    let client_secret = Entry::new(SERVICE, CLIENT_SECRET_ACCOUNT)
        .ok()
        .and_then(|entry| entry.get_password().ok());

    match (client_id, client_secret) {
        (Some(id), Some(secret)) => Some((id, secret)),
        _ => None,
    }
}

fn load_from_env() -> Option<(String, String)> {
    let client_id = std::env::var("DRIP_REDDIT_CLIENT_ID").ok();
    let client_secret = std::env::var("DRIP_REDDIT_CLIENT_SECRET").ok();

    match (client_id, client_secret) {
        (Some(id), Some(secret)) => Some((id, secret)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `DRIP_REDDIT_CLIENT_ID`/`DRIP_REDDIT_CLIENT_SECRET` are process-global
    // state; guard against any other test in this binary touching the same
    // vars concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn load_credentials_falls_back_to_env_vars_when_keyring_has_nothing() {
        let _guard = ENV_LOCK.lock().unwrap();

        // This machine has no working Secret Service / keychain backend, so
        // `load_from_keyring()` is expected to come back empty here -- that
        // failure must fall through silently to the env var path rather
        // than bubbling up as an error.
        std::env::set_var("DRIP_REDDIT_CLIENT_ID", "test-client-id");
        std::env::set_var("DRIP_REDDIT_CLIENT_SECRET", "test-client-secret");

        let result = load_credentials();

        std::env::remove_var("DRIP_REDDIT_CLIENT_ID");
        std::env::remove_var("DRIP_REDDIT_CLIENT_SECRET");

        let (client_id, client_secret) = result.expect("should fall back to env vars");
        assert_eq!(client_id, "test-client-id");
        assert_eq!(client_secret, "test-client-secret");
    }

    #[test]
    fn load_credentials_errors_clearly_when_nothing_is_configured() {
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::remove_var("DRIP_REDDIT_CLIENT_ID");
        std::env::remove_var("DRIP_REDDIT_CLIENT_SECRET");

        let err = load_credentials().expect_err("no credentials should be a clear error");
        let message = err.to_string();
        assert!(message.contains("drip init"));
        assert!(message.contains("DRIP_REDDIT_CLIENT_ID"));
        assert!(message.contains("reddit.com/prefs/apps"));
    }
}
