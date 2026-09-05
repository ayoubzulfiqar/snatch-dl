//! External tool discovery and self-installation.
//!
//! Snatch drives four external programs. Two of them can be installed without
//! root because upstream publishes a signed, checksummed standalone binary:
//!
//! | tool | source | root needed |
//! |------|--------|-------------|
//! | `aria2` | distribution package | yes |
//! | `ffmpeg` | distribution package | yes |
//! | `yt-dlp` | GitHub release (`yt-dlp_linux`) or distribution | no |
//! | `gallery-dl` | Codeberg release (`gallery-dl.bin`) | no |
//!
//! Snatch **never** invokes `sudo` itself. A GUI that silently escalates is a
//! GUI you cannot trust, and a password prompt driven by a download manager is
//! indistinguishable from malware. For the two that need root, Snatch shows
//! the exact command for the detected distribution and lets the user run it.
//!
//! Self-installed binaries land in a Snatch-owned directory rather than
//! `~/.local/bin`, so uninstalling Snatch cannot delete a tool the user
//! installed for their own use. That directory is prepended to `PATH` at
//! startup, so a copy Snatch installed takes precedence over an older
//! distribution package — and the dependency dialog says which copy is in use,
//! so the precedence is never a surprise.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Everything Snatch can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Aria2,
    Ffmpeg,
    YtDlp,
    GalleryDl,
    /// A JavaScript engine for yt-dlp to run a site's own player code in.
    ///
    /// Not something Snatch runs itself: it names whichever engine is
    /// installed to yt-dlp, which drives it. yt-dlp enables only Deno on its
    /// own, so a machine with node on it still extracts the deprecated way
    /// unless it is told -- and says so: "YouTube extraction without a JS
    /// runtime has been deprecated, and some formats may be missing". What
    /// that looks like from the outside is a video that offers only low
    /// qualities or no video at all, or a download that fails on a site which
    /// plays perfectly in the browser -- with nothing on screen to connect
    /// the two. Listing it here is the connection.
    JsRuntime,
    /// Resolves a live broadcast on the sites yt-dlp is weakest on.
    ///
    /// yt-dlp is built for archival -- work out the file and fetch it.
    /// streamlink is built for the opposite job: work out what a page is
    /// broadcasting right now. On Twitch, Kick, chzzk, SOOP and a hundred
    /// others that is the difference between a channel Snatch can record and
    /// one it reports as having nothing on it. Only ever used as a resolver:
    /// the recording is still ffmpeg's, so the stop button works the same.
    Streamlink,
}

impl Tool {
    pub const ALL: [Tool; 6] = [
        Tool::Aria2,
        Tool::Ffmpeg,
        Tool::YtDlp,
        Tool::GalleryDl,
        Tool::JsRuntime,
        Tool::Streamlink,
    ];

    pub fn binary(self) -> &'static str {
        match self {
            Tool::Aria2 => "aria2c",
            Tool::Ffmpeg => "ffmpeg",
            Tool::YtDlp => "yt-dlp",
            Tool::GalleryDl => "gallery-dl",
            Tool::JsRuntime => "deno",
            Tool::Streamlink => "streamlink",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tool::Aria2 => "aria2",
            Tool::Ffmpeg => "FFmpeg",
            Tool::YtDlp => "yt-dlp",
            Tool::GalleryDl => "gallery-dl",
            Tool::JsRuntime => "JavaScript engine",
            Tool::Streamlink => "Streamlink",
        }
    }

    /// What stops working without it.
    pub fn purpose(self) -> &'static str {
        match self {
            Tool::Aria2 => "HTTP and FTP downloads",
            Tool::Ffmpeg => "converting, trimming and extracting audio",
            Tool::YtDlp => "site video extraction",
            Tool::GalleryDl => "gallery scraping",
            Tool::JsRuntime => "YouTube and the other sites that need JavaScript run",
            Tool::Streamlink => "live broadcasts on Twitch, Kick and 130 other sites",
        }
    }

    /// Snatch cannot download anything at all without aria2.
    pub fn required(self) -> bool {
        matches!(self, Tool::Aria2)
    }

    /// Whether upstream ships a standalone binary Snatch can fetch itself.
    pub fn self_installable(self) -> bool {
        matches!(self, Tool::YtDlp | Tool::GalleryDl)
    }

    /// The distribution package name, which is not always the binary name.
    fn package(self, distro: Distro) -> &'static str {
        match (self, distro) {
            (Tool::Aria2, _) => "aria2",
            (Tool::Ffmpeg, _) => "ffmpeg",
            (Tool::YtDlp, _) => "yt-dlp",
            (Tool::GalleryDl, _) => "gallery-dl",
            (Tool::JsRuntime, _) => "deno",
            (Tool::Streamlink, _) => "streamlink",
        }
    }

    /// Every binary that satisfies this tool, best first.
    ///
    /// Only the JavaScript engine has more than one: yt-dlp drives four, and
    /// any of them does the job. Reporting "Deno: missing" on a machine with
    /// node on it would send the reader off to install something they do not
    /// need.
    pub fn candidates(self) -> &'static [&'static str] {
        match self {
            Tool::JsRuntime => &["deno", "node", "quickjs", "bun"],
            other => std::slice::from_ref(match other {
                Tool::Aria2 => &"aria2c",
                Tool::Ffmpeg => &"ffmpeg",
                Tool::YtDlp => &"yt-dlp",
                Tool::GalleryDl => &"gallery-dl",
                Tool::JsRuntime => &"deno",
                Tool::Streamlink => &"streamlink",
            }),
        }
    }

    fn version_args(self) -> &'static [&'static str] {
        match self {
            // aria2c and ffmpeg print a banner; the others print a bare version.
            Tool::Aria2 | Tool::Ffmpeg => &["--version"],
            Tool::YtDlp | Tool::GalleryDl | Tool::JsRuntime | Tool::Streamlink => &["--version"],
        }
    }
}

/// Enough of the host distribution to name a package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Fedora,
    Debian,
    Arch,
    Suse,
    Alpine,
    Unknown,
}

impl Distro {
    pub fn detect() -> Self {
        // Prefer the package manager actually present: a container may carry
        // an os-release that does not match its tooling.
        for (binary, distro) in [
            ("dnf", Distro::Fedora),
            ("apt-get", Distro::Debian),
            ("pacman", Distro::Arch),
            ("zypper", Distro::Suse),
            ("apk", Distro::Alpine),
        ] {
            if which(binary).is_some() {
                return distro;
            }
        }
        Distro::Unknown
    }

    fn install_prefix(self) -> Option<&'static str> {
        match self {
            Distro::Fedora => Some("sudo dnf install -y"),
            Distro::Debian => Some("sudo apt install -y"),
            Distro::Arch => Some("sudo pacman -S --needed"),
            Distro::Suse => Some("sudo zypper install -y"),
            Distro::Alpine => Some("sudo apk add"),
            Distro::Unknown => None,
        }
    }
}

/// What a survey found for one tool.
#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub tool: Tool,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    /// True when the copy in use is one Snatch installed.
    pub managed: bool,
}

impl ToolStatus {
    pub fn present(&self) -> bool {
        self.path.is_some()
    }

    /// The command a user must run themselves, when root is required.
    pub fn manual_command(&self, distro: Distro) -> Option<String> {
        // No JavaScript engine yt-dlp likes is in most distributions'
        // repositories, so the ordinary "install this package" line would
        // fail with "no match" on Fedora and Debian both. Deno's own
        // installer works everywhere and wants no root, so it is offered
        // instead of a command that does not. Node satisfies this just as
        // well if one is already installed -- `candidates` finds it.
        if self.tool == Tool::JsRuntime {
            return Some("curl -fsSL https://deno.land/install.sh | sh".to_owned());
        }
        // Nor does any distribution package streamlink. pipx is the way its
        // own documentation recommends, wants no root, and keeps it out of
        // the system Python -- which modern distributions refuse to let pip
        // write to anyway.
        if self.tool == Tool::Streamlink {
            return Some("pipx install streamlink".to_owned());
        }
        let prefix = distro.install_prefix()?;
        // Fedora keeps ffmpeg in RPM Fusion, so the bare command would fail
        // with a confusing "no match" rather than an actionable error.
        if self.tool == Tool::Ffmpeg && distro == Distro::Fedora {
            return Some(
                "sudo dnf install -y \
                 https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
                 && sudo dnf install -y ffmpeg"
                    .to_owned(),
            );
        }
        Some(format!("{prefix} {}", self.tool.package(distro)))
    }
}

/// Find an executable on `PATH`.
pub fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(binary))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Resolve a tool, preferring a Snatch-managed copy over `PATH`.
///
/// Engines call this so a self-installed binary is picked up without the user
/// having to touch `PATH`.
pub fn resolve(tool: Tool, managed_dir: &Path) -> Option<PathBuf> {
    for candidate in tool.candidates() {
        let managed = managed_dir.join(candidate);
        if is_executable(&managed) {
            return Some(managed);
        }
        if let Some(found) = which(candidate) {
            return Some(found);
        }
    }
    None
}

/// Ask a tool for its version, briefly.
async fn probe_version(path: &Path, tool: Tool) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(path)
            .args(tool.version_args())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }

    // aria2 and ffmpeg print "<name> version X"; take the version token.
    Some(match tool {
        Tool::Aria2 | Tool::Ffmpeg => first
            .split_whitespace()
            .nth(2)
            .unwrap_or(first)
            .trim_end_matches(',')
            .to_owned(),
        Tool::YtDlp | Tool::GalleryDl => first.to_owned(),
        // "deno 2.5.3", "v22.1.0" from node, "1.1.29" from bun.
        // "streamlink 8.5.0"
        Tool::Streamlink => first.split_whitespace().nth(1).unwrap_or(first).to_owned(),
        Tool::JsRuntime => first
            .split_whitespace()
            .find(|token| {
                token
                    .trim_start_matches('v')
                    .starts_with(|c: char| c.is_ascii_digit())
            })
            .unwrap_or(first)
            .to_owned(),
    })
}

/// Look at every tool Snatch can use.
pub async fn survey(managed_dir: &Path) -> Vec<ToolStatus> {
    let mut statuses = Vec::with_capacity(Tool::ALL.len());
    for tool in Tool::ALL {
        let path = resolve(tool, managed_dir);
        let managed = path
            .as_ref()
            .is_some_and(|found| found.starts_with(managed_dir));
        let version = match &path {
            Some(found) => probe_version(found, tool).await,
            None => None,
        };
        statuses.push(ToolStatus {
            tool,
            path,
            version,
            managed,
        });
    }
    statuses
}

// ---------------------------------------------------------------------------
// Self-installation
// ---------------------------------------------------------------------------

/// Progress while fetching a tool, so a 40 MB download is not a frozen dialog.
#[derive(Debug, Clone)]
pub enum InstallProgress {
    Resolving,
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Installed(PathBuf),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Where each self-installable tool comes from.
struct Source {
    /// A JSON API returning the latest release.
    api: &'static str,
    /// Asset holding the binary, chosen per architecture.
    binary_asset: &'static str,
    /// Asset holding `sha256  name` lines.
    checksum_asset: &'static str,
}

fn source_for(tool: Tool) -> Result<Source> {
    // Only x86_64 and aarch64 have published Linux builds.
    let arch = std::env::consts::ARCH;
    Ok(match tool {
        Tool::YtDlp => Source {
            api: "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest",
            binary_asset: match arch {
                "x86_64" => "yt-dlp_linux",
                "aarch64" => "yt-dlp_linux_aarch64",
                other => bail!("yt-dlp publishes no Linux binary for {other}"),
            },
            checksum_asset: "SHA2-256SUMS",
        },
        Tool::GalleryDl => {
            if arch != "x86_64" {
                bail!("gallery-dl publishes a standalone binary for x86_64 only");
            }
            Source {
                api: "https://codeberg.org/api/v1/repos/mikf/gallery-dl/releases/latest",
                binary_asset: "gallery-dl.bin",
                checksum_asset: "SHA256SUMS",
            }
        }
        other => bail!(
            "{} must come from your distribution; Snatch will not run sudo for you",
            other.title()
        ),
    })
}

/// Download, verify and install one self-installable tool.
///
/// The checksum is checked **before** the file is made executable, and the
/// binary is moved into place only after it verifies, so an interrupted
/// download can never leave a half-written executable behind.
pub async fn install(
    tool: Tool,
    managed_dir: &Path,
    report: impl Fn(InstallProgress),
) -> Result<PathBuf> {
    let source = source_for(tool)?;
    report(InstallProgress::Resolving);

    let http = wreq::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("Snatch/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not build the HTTP client")?;

    let release: Release = http
        .get(source.api)
        .send()
        .await
        .with_context(|| format!("could not reach the {} release API", tool.title()))?
        .error_for_status()
        .with_context(|| format!("the {} release API returned an error", tool.title()))?
        .json()
        .await
        .with_context(|| format!("could not parse the {} release metadata", tool.title()))?;

    let find = |name: &str| -> Option<&ReleaseAsset> {
        release.assets.iter().find(|asset| asset.name == name)
    };
    let binary_asset = find(source.binary_asset).with_context(|| {
        format!(
            "release {} has no asset called {}",
            release.tag_name, source.binary_asset
        )
    })?;
    let checksum_asset = find(source.checksum_asset).with_context(|| {
        format!(
            "release {} has no asset called {}",
            release.tag_name, source.checksum_asset
        )
    })?;

    log::info!(
        "installing {} {} from {}",
        tool.title(),
        release.tag_name,
        binary_asset.browser_download_url
    );

    // Checksums first: a tiny file, and there is no point downloading 40 MB if
    // the manifest does not even list the asset.
    let sums = http
        .get(&checksum_asset.browser_download_url)
        .send()
        .await
        .context("could not download the checksum file")?
        .error_for_status()
        .context("the checksum file returned an error")?
        .text()
        .await
        .context("could not read the checksum file")?;

    let expected = expected_digest(&sums, &binary_asset.name).with_context(|| {
        format!(
            "{} does not list a checksum for {}",
            source.checksum_asset, binary_asset.name
        )
    })?;

    let response = http
        .get(&binary_asset.browser_download_url)
        .send()
        .await
        .context("could not start the download")?
        .error_for_status()
        .context("the download returned an error")?;

    let total = response.content_length();
    let mut body = Vec::with_capacity(total.unwrap_or(8 << 20) as usize);
    report(InstallProgress::Downloading { received: 0, total });

    let mut stream = std::pin::pin!(response.bytes_stream());
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.context("the download was interrupted")?;
        body.extend_from_slice(&chunk);
        report(InstallProgress::Downloading {
            received: body.len() as u64,
            total,
        });
    }

    report(InstallProgress::Verifying);
    let actual = sha256_hex(&body);
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            binary_asset.name
        );
    }
    log::info!("sha256 verified for {}: {actual}", binary_asset.name);

    std::fs::create_dir_all(managed_dir)
        .with_context(|| format!("could not create {}", managed_dir.display()))?;
    let destination = managed_dir.join(tool.binary());
    let staging = managed_dir.join(format!(".{}.partial", tool.binary()));

    std::fs::write(&staging, &body)
        .with_context(|| format!("could not write {}", staging.display()))?;
    set_executable(&staging)?;
    // Rename last: the destination is either the old binary or the new one,
    // never a truncated file.
    std::fs::rename(&staging, &destination)
        .with_context(|| format!("could not install {}", destination.display()))?;

    report(InstallProgress::Installed(destination.clone()));
    Ok(destination)
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("could not stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("could not make {} executable", path.display()))
}

/// Pull one digest out of a `sha256sum`-style manifest.
///
/// Both projects use `<hex>  <name>`, but gallery-dl marks binary mode with a
/// `*` and yt-dlp's manifest lists every platform, so the name must match
/// exactly rather than by prefix.
fn expected_digest(manifest: &str, asset: &str) -> Option<String> {
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let Some(name) = parts.next() else { continue };
        let name = name.trim_start_matches('*');
        if name == asset && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

/// SHA-256 without pulling in a hashing crate: this is the only digest Snatch
/// computes, and the implementation is short enough to audit.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut hash: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    // Padding guarantees a whole number of 64-byte blocks, so the remainder
    // both chunkings produce is always empty.
    let (blocks, _) = message.as_chunks::<64>();
    for block in blocks {
        let mut w = [0u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (index, word) in words.iter().enumerate() {
            w[index] = u32::from_be_bytes(*word);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in hash.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    hash.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Spans a block boundary, which is where a padding bug would show.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let long = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&long),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn reads_a_digest_out_of_each_projects_manifest() {
        // yt-dlp's manifest lists every platform, so a prefix match would
        // return the wrong line.
        let ytdlp = "\
aaaa000000000000000000000000000000000000000000000000000000000000  yt-dlp
bbbb111111111111111111111111111111111111111111111111111111111111  yt-dlp_linux
cccc222222222222222222222222222222222222222222222222222222222222  yt-dlp_linux_aarch64";
        assert_eq!(
            expected_digest(ytdlp, "yt-dlp_linux").as_deref(),
            Some("bbbb111111111111111111111111111111111111111111111111111111111111")
        );
        assert_eq!(
            expected_digest(ytdlp, "yt-dlp_linux_aarch64").as_deref(),
            Some("cccc222222222222222222222222222222222222222222222222222222222222")
        );

        // gallery-dl marks binary mode with a leading asterisk.
        let gallery =
            "dddd333333333333333333333333333333333333333333333333333333333333 *gallery-dl.bin";
        assert_eq!(
            expected_digest(gallery, "gallery-dl.bin").as_deref(),
            Some("dddd333333333333333333333333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn rejects_a_manifest_without_the_asset() {
        let manifest = "aaaa  something-else";
        assert_eq!(expected_digest(manifest, "yt-dlp_linux"), None);
        // A short or non-hex field is not a digest.
        assert_eq!(
            expected_digest("nothex  yt-dlp_linux", "yt-dlp_linux"),
            None
        );
        assert_eq!(expected_digest("", "yt-dlp_linux"), None);
    }

    #[test]
    fn only_the_standalone_tools_are_self_installable() {
        assert!(Tool::YtDlp.self_installable());
        assert!(Tool::GalleryDl.self_installable());
        // Snatch must never try to install these: they need root.
        assert!(!Tool::Aria2.self_installable());
        assert!(!Tool::Ffmpeg.self_installable());
        assert!(source_for(Tool::Aria2).is_err());
        assert!(source_for(Tool::Ffmpeg).is_err());
    }

    #[test]
    fn fedora_points_ffmpeg_at_rpm_fusion() {
        let status = ToolStatus {
            tool: Tool::Ffmpeg,
            path: None,
            version: None,
            managed: false,
        };
        // A bare `dnf install ffmpeg` fails on stock Fedora.
        let command = status
            .manual_command(Distro::Fedora)
            .expect("Fedora has a command");
        assert!(command.contains("rpmfusion"), "{command}");

        let debian = status
            .manual_command(Distro::Debian)
            .expect("Debian has a command");
        assert_eq!(debian, "sudo apt install -y ffmpeg");
    }

    #[test]
    fn only_aria2_is_required() {
        assert!(Tool::Aria2.required());
        for tool in [Tool::Ffmpeg, Tool::YtDlp, Tool::GalleryDl] {
            assert!(!tool.required(), "{} should be optional", tool.title());
        }
    }
}
