//! Well-known filesystem locations. Everything Snatch owns lives under a single
//! XDG data directory so uninstalling is `rm -rf` on one path.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub const APP_ID: &str = "com.snatch.dl";

/// `$XDG_DATA_HOME/snatch-dl`, created on demand.
pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("could not determine the XDG data directory (is $HOME set?)")?
        .join("snatch-dl");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

/// The socket `snatch-nmh` connects to.
pub fn socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("snatch.sock"))
}

/// aria2's session file, so queued and partial downloads survive a restart.
pub fn session_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("aria2.session"))
}

/// The proxy table managed by [`crate::network::ProxyManager`].
pub fn proxy_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("proxies.json"))
}

/// The SQLite job history.
pub fn database_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("snatch.sqlite"))
}

/// Persistent state for the BitTorrent session (resume data, DHT table).
pub fn torrent_state_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("torrents"))
}

/// User settings.
pub fn settings_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("settings.json"))
}

/// Binaries Snatch installed for itself.
///
/// Deliberately not `~/.local/bin`: uninstalling Snatch must never remove a
/// tool the user installed for their own use, and a distribution package
/// should always take precedence over our copy.
pub fn managed_bin_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("bin"))
}

/// Where finished files land: the XDG download directory, falling back to `$HOME`.
pub fn download_dir() -> Result<PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .context("could not determine a download directory (is $HOME set?)")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}
