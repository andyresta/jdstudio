// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! API keys, kept in the system's own credential store - Windows'
//! Credential Manager, the macOS keychain, the Linux kernel keyring - and
//! never in the preferences file, where a backup or a shared folder would
//! carry them off. One entry per provider, named by its preset id.

/// The service the entries are filed under.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const SERVICE: &str = "JDStudio assistant";

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn entry(provider: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, provider).map_err(|error| error.to_string())
}

/// The key saved for `provider`, if there is one.
pub fn get(provider: &str) -> Option<String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        entry(provider)
            .ok()?
            .get_password()
            .ok()
            .filter(|key| !key.is_empty())
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = provider;
        None
    }
}

/// Saves `key` for `provider`, replacing any before it.
pub fn set(provider: &str, key: &str) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        entry(provider)?
            .set_password(key.trim())
            .map_err(|error| error.to_string())
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (provider, key);
        Err("keys are not stored on this device".to_owned())
    }
}

/// Forgets the key for `provider`.
pub fn remove(provider: &str) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        match entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = provider;
        Ok(())
    }
}
