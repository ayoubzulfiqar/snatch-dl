//! User-configurable settings, persisted as JSON.
//!
//! Where a setting can be applied matters, and the three cases behave very
//! differently:
//!
//! * **Live, globally.** aria2's `changeGlobalOption` accepts only a short
//!   list — concurrency and the overall speed caps among them. Those take
//!   effect immediately.
//! * **Per download, at add time.** `split`, `max-connection-per-server` and
//!   friends are per-download options. Changing them affects new downloads;
//!   ones already running keep the settings they were started with.
//! * **At spawn only.** `file-allocation`, `auto-save-interval` and the disk
//!   cache are read when aria2 starts, so changing them needs a restart.
//!
//! The Settings page states which is which rather than pretending everything
//! is instant.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Which program performs plain HTTP/FTP downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HttpEngine {
    /// aria2 with segmented connections. The default and the fastest.
    #[default]
    Aria2,
    /// Wget2, which is also multithreaded. Useful when aria2 is unavailable
    /// or a site behaves badly with aria2's connection pattern.
    Wget,
}

impl HttpEngine {
    pub const ALL: [HttpEngine; 2] = [HttpEngine::Aria2, HttpEngine::Wget];

    pub fn label(self) -> &'static str {
        match self {
            HttpEngine::Aria2 => "aria2 (segmented, recommended)",
            HttpEngine::Wget => "Wget2 (multithreaded)",
        }
    }
}

/// How aria2 reserves space for a file before writing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Allocation {
    /// No preallocation. Fastest to start, most prone to fragmentation.
    None,
    /// `fallocate`: instant on ext4, XFS and Btrfs. The sane default.
    #[default]
    Falloc,
    /// Write zeroes. Portable but slow on a large file.
    Prealloc,
}

impl Allocation {
    pub const ALL: [Allocation; 3] = [Allocation::None, Allocation::Falloc, Allocation::Prealloc];

    pub fn label(self) -> &'static str {
        match self {
            Allocation::None => "None — start instantly",
            Allocation::Falloc => "fallocate — instant on ext4, XFS, Btrfs",
            Allocation::Prealloc => "Preallocate — portable but slow",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Allocation::None => "none",
            Allocation::Falloc => "falloc",
            Allocation::Prealloc => "prealloc",
        }
    }
}

/// Download tuning. The defaults are what IDM-style segmenting looks like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DownloadSettings {
    pub engine: HttpEngine,
    /// Segments per download. aria2 caps this at 16 per server.
    pub split: u32,
    /// Simultaneous connections to one host.
    pub connections_per_server: u32,
    /// Never split a file smaller than this many mebibytes.
    pub min_split_mib: u32,
    /// Downloads running at once.
    pub concurrent_downloads: u32,
    /// Overall cap in KiB/s. 0 means unlimited.
    pub max_overall_down_kib: u64,
    /// Per-download cap in KiB/s. 0 means unlimited.
    pub max_per_download_kib: u64,
    pub retries: u32,
    pub retry_wait_seconds: u32,
    pub allocation: Allocation,
    /// Verify TLS certificates. Turning this off is a real risk.
    pub check_certificate: bool,
    /// Seconds between control-file writes. 0 disables periodic writes, so no
    /// `.aria2` file appears while downloading — at the cost of losing resume
    /// data if the machine loses power mid-download.
    pub auto_save_interval: u32,
    pub user_agent: String,
    /// File the download into a subfolder chosen by its type, instead of
    /// dropping everything into one directory.
    pub categorise: bool,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            engine: HttpEngine::default(),
            split: 16,
            connections_per_server: 16,
            min_split_mib: 1,
            concurrent_downloads: 5,
            max_overall_down_kib: 0,
            max_per_download_kib: 0,
            retries: 5,
            retry_wait_seconds: 3,
            allocation: Allocation::default(),
            check_certificate: true,
            auto_save_interval: 60,
            user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"
                .to_owned(),
            categorise: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TorrentSettings {
    /// Overall upload cap in KiB/s. 0 means unlimited.
    pub max_upload_kib: u64,
    /// Stop seeding once this ratio is reached. 0 means seed indefinitely.
    pub seed_ratio: f64,
    pub enable_dht: bool,
    /// Accept incoming peers. Disabled automatically when a SOCKS5 proxy is
    /// set, because a public listener would advertise the real address.
    pub accept_incoming: bool,
    pub max_peers: u32,
}

impl Default for TorrentSettings {
    fn default() -> Self {
        Self {
            max_upload_kib: 0,
            seed_ratio: 0.0,
            enable_dht: true,
            accept_incoming: true,
            max_peers: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct MediaSettings {
    /// Embed metadata and chapters in extracted video.
    pub embed_metadata: bool,
    pub write_subtitles: bool,
    /// MP3 bitrate used by "Extract Audio".
    pub audio_bitrate_kbps: u32,
    /// Write `info.json` beside scraped galleries.
    pub gallery_metadata: bool,
    /// Re-download gallery files that already exist.
    pub gallery_overwrite: bool,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            embed_metadata: true,
            write_subtitles: false,
            audio_bitrate_kbps: 192,
            gallery_metadata: true,
            gallery_overwrite: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct InterfaceSettings {
    /// Show a desktop notification when a download finishes.
    pub notify_on_finish: bool,
    /// Bring the window forward when the browser hands over a job.
    pub raise_on_capture: bool,
    /// Confirm before cancelling a running download.
    pub confirm_cancel: bool,
    /// Override the download directory. Empty means use the XDG one.
    pub download_dir: String,
    /// The page shown at startup, so Snatch reopens where you left it.
    pub last_page: String,
    /// Whether the sidebar drawer is open. Remembered across restarts, and
    /// never closed automatically.
    pub sidebar_open: bool,
}

impl Default for InterfaceSettings {
    fn default() -> Self {
        Self {
            notify_on_finish: true,
            raise_on_capture: true,
            confirm_cancel: true,
            download_dir: String::new(),
            last_page: "downloads".to_owned(),
            sidebar_open: true,
        }
    }
}

/// Everything the user can configure.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    pub download: DownloadSettings,
    pub torrent: TorrentSettings,
    pub media: MediaSettings,
    pub interface: InterfaceSettings,
}

impl Settings {
    /// Load, tolerating a missing or corrupt file.
    ///
    /// A broken settings file must never stop the application starting: the
    /// user would have no way to fix it from inside the app.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(settings) => settings.clamped(),
                Err(error) => {
                    log::warn!("ignoring malformed {}: {error}", path.display());
                    Settings::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                log::warn!("could not read {}: {error}", path.display());
                Settings::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("the settings file has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let bytes = serde_json::to_vec_pretty(self).context("could not encode settings")?;

        // Write and rename, so a crash cannot truncate the file.
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &bytes)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    }

    /// Force every value into a range the engines accept.
    ///
    /// A hand-edited file, or an old file from a version with different
    /// bounds, must not produce an aria2 command line it rejects.
    pub fn clamped(mut self) -> Self {
        let download = &mut self.download;
        // aria2 refuses more than 16 connections to one server.
        download.split = download.split.clamp(1, 64);
        download.connections_per_server = download.connections_per_server.clamp(1, 16);
        download.min_split_mib = download.min_split_mib.clamp(1, 1024);
        download.concurrent_downloads = download.concurrent_downloads.clamp(1, 50);
        download.retries = download.retries.min(60);
        download.retry_wait_seconds = download.retry_wait_seconds.min(600);
        // aria2 only accepts 0..=600 here.
        download.auto_save_interval = download.auto_save_interval.min(600);
        if download.user_agent.trim().is_empty() {
            download.user_agent = DownloadSettings::default().user_agent;
        }

        self.torrent.max_peers = self.torrent.max_peers.clamp(1, 1000);
        if !self.torrent.seed_ratio.is_finite() || self.torrent.seed_ratio < 0.0 {
            self.torrent.seed_ratio = 0.0;
        }

        self.media.audio_bitrate_kbps = self.media.audio_bitrate_kbps.clamp(64, 320);
        self
    }

    /// The download directory, or `None` to use the XDG one.
    pub fn download_dir_override(&self) -> Option<PathBuf> {
        let value = self.interface.download_dir.trim();
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    }

    /// aria2 command-line arguments that can only be set when it starts.
    pub fn aria2_spawn_args(&self) -> Vec<String> {
        let download = &self.download;
        vec![
            format!(
                "--max-concurrent-downloads={}",
                download.concurrent_downloads
            ),
            format!("--split={}", download.split),
            format!(
                "--max-connection-per-server={}",
                download.connections_per_server
            ),
            format!("--min-split-size={}M", download.min_split_mib),
            format!("--file-allocation={}", download.allocation.as_str()),
            format!("--max-tries={}", download.retries),
            format!("--retry-wait={}", download.retry_wait_seconds),
            format!("--check-certificate={}", download.check_certificate),
            format!("--auto-save-interval={}", download.auto_save_interval),
            format!("--user-agent={}", download.user_agent),
            format!(
                "--max-overall-download-limit={}",
                kib_to_bytes(download.max_overall_down_kib)
            ),
            format!(
                "--max-download-limit={}",
                kib_to_bytes(download.max_per_download_kib)
            ),
            format!(
                "--max-overall-upload-limit={}",
                kib_to_bytes(self.torrent.max_upload_kib)
            ),
        ]
    }

    /// Options aria2 accepts through `changeGlobalOption` while running.
    ///
    /// Deliberately short: aria2 rejects the call outright if it contains an
    /// option that is not globally changeable, so a speculative extra entry
    /// would break every live update.
    pub fn aria2_live_options(&self) -> Vec<(String, String)> {
        vec![
            (
                "max-concurrent-downloads".to_owned(),
                self.download.concurrent_downloads.to_string(),
            ),
            (
                "max-overall-download-limit".to_owned(),
                kib_to_bytes(self.download.max_overall_down_kib),
            ),
            (
                "max-overall-upload-limit".to_owned(),
                kib_to_bytes(self.torrent.max_upload_kib),
            ),
        ]
    }

    /// Per-download options passed with every `addUri`.
    pub fn aria2_download_options(&self) -> Vec<(String, String)> {
        let download = &self.download;
        vec![
            ("split".to_owned(), download.split.to_string()),
            (
                "max-connection-per-server".to_owned(),
                download.connections_per_server.to_string(),
            ),
            (
                "min-split-size".to_owned(),
                format!("{}M", download.min_split_mib),
            ),
            (
                "max-download-limit".to_owned(),
                kib_to_bytes(download.max_per_download_kib),
            ),
        ]
    }
}

/// The folder a file of this kind belongs in.
///
/// Names match what people expect from a download manager, and the sort is by
/// extension because that is all that is known before the transfer starts.
pub fn category_for(filename: &str) -> Option<&'static str> {
    let extension = filename.rsplit_once('.').map(|(_, ext)| ext)?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "ts"
        | "ogv" | "3gp" => "Video",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" | "aiff" | "mka" => "Music",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "jxl" | "svg" | "tif"
        | "tiff" | "heic" => "Images",
        "pdf" | "epub" | "mobi" | "azw3" | "doc" | "docx" | "odt" | "rtf" | "txt" | "xls"
        | "xlsx" | "ods" | "ppt" | "pptx" | "odp" | "djvu" | "cbz" | "cbr" => "Documents",
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" => "Compressed",
        "deb" | "rpm" | "apk" | "dmg" | "exe" | "msi" | "appimage" | "snap" | "flatpak" | "iso"
        | "img" | "run" => "Programs",
        // An unknown type is left in the root rather than filed under a
        // guess: a wrong folder is worse than no folder.
        _ => return None,
    })
}

/// aria2 wants bytes per second; `0` means unlimited in both.
fn kib_to_bytes(kib: u64) -> String {
    kib.saturating_mul(1024).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_idm_style_segmented() {
        let settings = Settings::default();
        assert_eq!(settings.download.split, 16);
        assert_eq!(settings.download.connections_per_server, 16);
        assert_eq!(settings.download.engine, HttpEngine::Aria2);
        assert!(settings.download.check_certificate);
    }

    #[test]
    fn clamping_keeps_aria2_from_rejecting_the_command_line() {
        let mut settings = Settings::default();
        // aria2 refuses more than 16 connections per server and caps
        // auto-save-interval at 600; a hand-edited file must not break startup.
        settings.download.connections_per_server = 999;
        settings.download.auto_save_interval = 99_999;
        settings.download.split = 0;
        settings.download.concurrent_downloads = 0;
        settings.media.audio_bitrate_kbps = 5;
        settings.torrent.seed_ratio = f64::NAN;

        let settings = settings.clamped();
        assert_eq!(settings.download.connections_per_server, 16);
        assert_eq!(settings.download.auto_save_interval, 600);
        assert_eq!(settings.download.split, 1);
        assert_eq!(settings.download.concurrent_downloads, 1);
        assert_eq!(settings.media.audio_bitrate_kbps, 64);
        assert_eq!(settings.torrent.seed_ratio, 0.0);
    }

    #[test]
    fn an_empty_user_agent_falls_back_rather_than_sending_nothing() {
        let mut settings = Settings::default();
        settings.download.user_agent = "   ".to_owned();
        assert!(!settings.clamped().download.user_agent.trim().is_empty());
    }

    #[test]
    fn speed_limits_convert_to_the_bytes_aria2_expects() {
        let mut settings = Settings::default();
        settings.download.max_overall_down_kib = 500;
        let args = settings.aria2_spawn_args();
        assert!(
            args.contains(&"--max-overall-download-limit=512000".to_owned()),
            "{args:?}"
        );
        // Zero must stay zero: aria2 reads that as unlimited.
        assert!(
            args.contains(&"--max-download-limit=0".to_owned()),
            "{args:?}"
        );
    }

    #[test]
    fn live_options_only_list_what_aria2_accepts_globally() {
        // aria2 rejects the whole changeGlobalOption call if any key is not
        // globally changeable, so this list must stay conservative.
        let allowed = [
            "max-concurrent-downloads",
            "max-overall-download-limit",
            "max-overall-upload-limit",
        ];
        for (key, _) in Settings::default().aria2_live_options() {
            assert!(
                allowed.contains(&key.as_str()),
                "{key} is not globally changeable"
            );
        }
    }

    #[test]
    fn per_download_options_carry_the_segmenting() {
        let options = Settings::default().aria2_download_options();
        let split = options
            .iter()
            .find(|(key, _)| key == "split")
            .map(|(_, value)| value.clone());
        assert_eq!(split.as_deref(), Some("16"));
    }

    #[test]
    fn disabling_the_control_file_is_expressible() {
        let mut settings = Settings::default();
        // 0 stops aria2 writing a .aria2 file during the download.
        settings.download.auto_save_interval = 0;
        assert!(
            settings
                .aria2_spawn_args()
                .contains(&"--auto-save-interval=0".to_owned())
        );
    }

    #[test]
    fn a_corrupt_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join("snatch-settings-corrupt.json");
        std::fs::write(&path, b"{not json").expect("write");
        assert_eq!(Settings::load(&path), Settings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let path = std::env::temp_dir().join("snatch-settings-roundtrip.json");
        let _ = std::fs::remove_file(&path);

        let mut settings = Settings::default();
        settings.download.split = 8;
        settings.download.engine = HttpEngine::Wget;
        settings.interface.download_dir = "/tmp/somewhere".to_owned();
        settings.save(&path).expect("save works");

        let loaded = Settings::load(&path);
        assert_eq!(loaded.download.split, 8);
        assert_eq!(loaded.download.engine, HttpEngine::Wget);
        assert_eq!(
            loaded.download_dir_override(),
            Some(PathBuf::from("/tmp/somewhere"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_partial_file_keeps_defaults_for_absent_keys() {
        // Forward compatibility: an old settings file must still load.
        let path = std::env::temp_dir().join("snatch-settings-partial.json");
        std::fs::write(&path, br#"{"download":{"split":4}}"#).expect("write");
        let loaded = Settings::load(&path);
        assert_eq!(loaded.download.split, 4);
        assert_eq!(
            loaded.download.connections_per_server,
            DownloadSettings::default().connections_per_server
        );
        assert_eq!(loaded.torrent, TorrentSettings::default());
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    #[test]
    fn files_are_sorted_the_way_people_expect() {
        assert_eq!(category_for("movie.mkv"), Some("Video"));
        assert_eq!(category_for("Song.FLAC"), Some("Music"));
        assert_eq!(category_for("photo.jpeg"), Some("Images"));
        assert_eq!(category_for("manual.pdf"), Some("Documents"));
        assert_eq!(category_for("release.tar.gz"), Some("Compressed"));
        assert_eq!(category_for("app.AppImage"), Some("Programs"));
        assert_eq!(category_for("ubuntu-24.04.iso"), Some("Programs"));
    }

    #[test]
    fn an_unknown_type_is_left_in_the_root() {
        // A wrong folder is worse than no folder: the user cannot find it.
        assert_eq!(category_for("data.qqq"), None);
        assert_eq!(category_for("no-extension"), None);
        assert_eq!(category_for(""), None);
    }
}
