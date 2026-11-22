//! OS keychain storage for the iroh secret: turned off in DeskBeam.
//!
//! DashBeam kept the secret in one keychain entry per user account and copied it into a data
//! directory that had no `identity.key`. That entry only existed to migrate DashBeam's older
//! installs, which DeskBeam has none of. Kept on, it would give every new data directory (a
//! second `deskbeam-cli --data-dir`, a test) the identity of the first one. So `identity.key`
//! in the data directory is the only store, and these are the no-ops DashBeam used on Android.

pub fn load_secret_hex() -> anyhow::Result<Option<String>> {
    Ok(None)
}

pub fn save_secret_hex(_hex: &str) -> anyhow::Result<()> {
    Ok(())
}

pub fn delete_secret() -> anyhow::Result<()> {
    Ok(())
}
