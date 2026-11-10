//! Connection passwords in the system keychain: the Secret Service on Linux, the
//! Credential Manager on Windows, the Keychain on macOS. Each saved connection has one
//! entry, keyed by its id.
//!
//! These calls block on the keychain, which can take a moment or show an unlock prompt,
//! so the app makes them off the UI thread.

const SERVICE: &str = "dev.khaodius.deltos";

fn entry(connection_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, connection_id).map_err(|e| e.to_string())
}

/// The saved password, or `None` when there is none.
pub fn password(connection_id: &str) -> Result<Option<String>, String> {
    match entry(connection_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn set_password(connection_id: &str, password: &str) -> Result<(), String> {
    entry(connection_id)?
        .set_password(password)
        .map_err(|e| e.to_string())
}

/// Removes the saved password. Removing one that is not there is not an error.
pub fn delete_password(connection_id: &str) -> Result<(), String> {
    match entry(connection_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
