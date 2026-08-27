//! Verifying that a finished download is the file it claimed to be.
//!
//! Two halves, and the second is the interesting one:
//!
//! * **Parsing.** People paste a hash in whatever form the download page
//!   printed it — bare hex, `sha256:…`, a coreutils `SHA256SUMS` line, or the
//!   BSD `SHA256 (name) = …` form. All of them are accepted and normalised to
//!   aria2's `<type>=<digest>` spelling.
//! * **Discovery.** Almost every project that publishes a file also publishes
//!   its digest right next to it, and nobody reads them. Given a download URL
//!   Snatch probes the handful of conventional locations, parses whatever it
//!   finds and pulls out the line for this exact file — so verification costs
//!   the user nothing.
//!
//! aria2 verifies a whole-file hash itself, so for the default engine this is
//! one option string and a failed hash aborts the download. The wget engine
//! has no equivalent, so the file is hashed afterwards by streaming it through
//! the matching coreutils tool — the in-tree SHA-256 takes a whole buffer and
//! would have to hold a multi-gigabyte download in memory.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Checksum files are tiny; anything larger is not one.
const MAX_SUMS_BYTES: usize = 512 * 1024;

/// A hash algorithm both aria2 and coreutils know.
///
/// aria2 also offers adler32, which is deliberately absent: nothing publishes
/// an adler32 of a release file, and it is not a digest anyone should trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    /// How aria2 spells it in `--checksum=<type>=<digest>`.
    pub fn aria2_name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha-1",
            Self::Sha224 => "sha-224",
            Self::Sha256 => "sha-256",
            Self::Sha384 => "sha-384",
            Self::Sha512 => "sha-512",
        }
    }

    /// The coreutils program that computes it.
    fn tool(self) -> &'static str {
        match self {
            Self::Md5 => "md5sum",
            Self::Sha1 => "sha1sum",
            Self::Sha224 => "sha224sum",
            Self::Sha256 => "sha256sum",
            Self::Sha384 => "sha384sum",
            Self::Sha512 => "sha512sum",
        }
    }

    /// How many hex characters its digest has.
    fn hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha224 => 56,
            Self::Sha256 => 64,
            Self::Sha384 => 96,
            Self::Sha512 => 128,
        }
    }

    /// Identify an algorithm from a digest length.
    ///
    /// The six lengths are all distinct, so a bare hash needs no label.
    fn from_hex_len(len: usize) -> Option<Self> {
        [
            Self::Md5,
            Self::Sha1,
            Self::Sha224,
            Self::Sha256,
            Self::Sha384,
            Self::Sha512,
        ]
        .into_iter()
        .find(|algorithm| algorithm.hex_len() == len)
    }

    /// Identify an algorithm from a written name, in any of its spellings.
    fn from_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase().replace(['-', '_'], "");
        match name.as_str() {
            "md5" => Some(Self::Md5),
            "sha1" => Some(Self::Sha1),
            "sha224" => Some(Self::Sha224),
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

/// One digest, and what produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algorithm: Algorithm,
    /// Lowercase hex, already known to be the right length.
    pub hex: String,
}

impl Checksum {
    /// The value for aria2's `checksum` option.
    pub fn aria2_value(&self) -> String {
        format!("{}={}", self.algorithm.aria2_name(), self.hex)
    }

    /// A short label for the interface.
    pub fn label(&self) -> String {
        let short: String = self.hex.chars().take(12).collect();
        format!("{} {short}…", self.algorithm.aria2_name())
    }
}

/// Is this a run of hex digits?
fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Read a hash out of whatever a download page printed.
///
/// Accepts a bare digest, `sha256:…` and `sha-256=…`, the coreutils
/// `<digest>  <name>` line and the BSD `SHA256 (name) = <digest>` line, so
/// pasting the whole line from a release page works without editing it down.
pub fn parse(input: &str) -> Option<Checksum> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }

    // BSD: `SHA256 (archive.tar.gz) = abc123…`
    if let Some((head, digest)) = text.rsplit_once('=')
        && let Some((name, _)) = head.split_once('(')
        && let Some(algorithm) = Algorithm::from_name(name)
    {
        return build(algorithm, digest);
    }

    // Labelled: `sha256:abc…`, `sha-256=abc…`.
    for separator in [':', '='] {
        if let Some((name, digest)) = text.split_once(separator)
            && let Some(algorithm) = Algorithm::from_name(name)
        {
            return build(algorithm, digest);
        }
    }

    // coreutils: `abc123…  archive.tar.gz`, or just the digest on its own.
    // The binary-mode `*` prefix belongs to the name, not the digest.
    let digest = text.split_whitespace().next()?;
    let algorithm = Algorithm::from_hex_len(digest.len())?;
    build(algorithm, digest)
}

fn build(algorithm: Algorithm, digest: &str) -> Option<Checksum> {
    let hex = digest.trim().to_ascii_lowercase();
    (is_hex(&hex) && hex.len() == algorithm.hex_len()).then_some(Checksum { algorithm, hex })
}

/// Pull the line for one file out of a checksum file.
///
/// Both conventional layouts appear in the wild, often in the same project:
/// the coreutils `<digest>  <name>` and the BSD `SHA256 (name) = <digest>`.
/// The name is matched on its last path component, because a `SHA256SUMS`
/// generated in a subdirectory carries the path it was generated with.
pub fn find_in_sums(body: &str, filename: &str) -> Option<Checksum> {
    let target = filename.trim();
    if target.is_empty() {
        return None;
    }

    let mut fallback = None;
    for line in body.lines().take(20_000) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (name, checksum) = if let Some((head, digest)) = line.rsplit_once('=')
            && let Some((algorithm, rest)) = head.split_once('(')
            && let Some(algorithm) = Algorithm::from_name(algorithm)
        {
            // BSD.
            let name = rest.trim().trim_end_matches(')').trim();
            (name.to_owned(), build(algorithm, digest))
        } else {
            // coreutils. The name may itself contain spaces, so it is
            // everything after the digest rather than the second field.
            let Some((digest, rest)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let name = rest.trim_start().trim_start_matches('*').trim();
            let checksum = Algorithm::from_hex_len(digest.len())
                .and_then(|algorithm| build(algorithm, digest));
            (name.to_owned(), checksum)
        };

        let Some(checksum) = checksum else { continue };
        let base = name.rsplit(['/', '\\']).next().unwrap_or(&name);
        if base == target {
            return Some(checksum);
        }
        // A case-insensitive match is better than nothing, but an exact one
        // wins, so keep looking.
        if fallback.is_none() && base.eq_ignore_ascii_case(target) {
            fallback = Some(checksum);
        }
    }
    fallback
}

/// Where a checksum for this file conventionally lives, in two waves.
///
/// The first wave is digests published for exactly this file, the second is
/// the directory-wide sums files. They are probed a wave at a time so a hit on
/// the specific one costs a single round trip and never fires the other eight
/// requests at somebody's server.
pub fn candidate_waves(download_url: &str) -> Vec<Vec<String>> {
    let all = candidate_urls(download_url);
    if all.is_empty() {
        return Vec::new();
    }
    let (specific, generic): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|candidate| SPECIFIC_SUFFIXES.iter().any(|s| candidate.ends_with(s)));
    [specific, generic]
        .into_iter()
        .filter(|wave| !wave.is_empty())
        .collect()
}

/// Suffixes that name one file's digest rather than a whole directory's.
const SPECIFIC_SUFFIXES: [&str; 4] = [".sha256", ".sha512", ".sha1", ".md5"];

/// Where a checksum for this file conventionally lives.
///
/// Ordered by how specific each candidate is: a digest published for exactly
/// this file is worth more than a `SHA256SUMS` covering a whole directory,
/// because the latter might not mention it at all.
pub fn candidate_urls(download_url: &str) -> Vec<String> {
    let Ok(base) = url::Url::parse(download_url.trim()) else {
        return Vec::new();
    };
    if !matches!(base.scheme(), "http" | "https") {
        return Vec::new();
    }
    let Some(filename) = base
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(str::to_owned)
    else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    let mut push = |relative: &str| {
        if let Ok(joined) = base.join(relative) {
            let text = joined.to_string();
            if !candidates.contains(&text) {
                candidates.push(text);
            }
        }
    };

    for suffix in ["sha256", "sha512", "sha1", "md5"] {
        push(&format!("{filename}.{suffix}"));
    }
    for sibling in [
        "SHA256SUMS",
        "SHA256SUMS.txt",
        "sha256sum.txt",
        "SHA512SUMS",
        "CHECKSUM",
        "CHECKSUMS",
        "checksums.txt",
        "MD5SUMS",
    ] {
        push(sibling);
    }
    candidates
}

/// Fetch one candidate and pull this file's digest out of it.
async fn fetch_candidate(
    client: &reqwest::Client,
    candidate: &str,
    filename: &str,
) -> Option<Checksum> {
    let response = client.get(candidate).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    // A checksum file is a few kilobytes. Anything bigger is a stylish 404
    // page, or a server that ignores the path and returns the site.
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_SUMS_BYTES)
    {
        return None;
    }
    let body = response.text().await.ok()?;
    if body.len() > MAX_SUMS_BYTES {
        return None;
    }

    // A `<file>.sha256` beside the download usually holds the digest alone,
    // with no name to match against.
    find_in_sums(&body, filename).or_else(|| {
        SPECIFIC_SUFFIXES
            .iter()
            .any(|suffix| candidate.ends_with(suffix))
            .then(|| parse(&body))
            .flatten()
    })
}

/// Look for a published digest for this download.
///
/// A miss is not an error: most downloads have no published digest, and
/// complaining on every add would be noise. Within a wave the candidates are
/// fetched concurrently and the highest-priority hit wins, so the whole thing
/// costs one round trip in the common case and two at worst.
pub async fn discover(
    client: &reqwest::Client,
    download_url: &str,
    filename: &str,
) -> Option<(Checksum, String)> {
    for wave in candidate_waves(download_url) {
        let found = futures::future::join_all(
            wave.iter()
                .map(|candidate| fetch_candidate(client, candidate, filename)),
        )
        .await;
        // Zip back against the wave so the reported URL is the one that hit,
        // and so priority within the wave still decides.
        if let Some((checksum, candidate)) = found
            .into_iter()
            .zip(wave.iter())
            .find_map(|(found, candidate)| found.map(|checksum| (checksum, candidate)))
        {
            log::info!(
                "found a {} digest for {filename} at {candidate}",
                checksum.algorithm.aria2_name()
            );
            return Some((checksum, candidate.clone()));
        }
    }
    None
}

/// Hash a file on disk and compare, for engines that cannot do it themselves.
///
/// Streams through the coreutils tool rather than reading the file in: a
/// download is exactly the kind of file that does not fit in memory.
pub async fn verify_file(path: &Path, expected: &Checksum) -> Result<()> {
    let tool = expected.algorithm.tool();
    let output = Command::new(tool)
        .arg("--binary")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("could not run {tool} to verify the download"))?;

    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        bail!("{tool} could not read the file: {}", reason.trim());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let actual = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual != expected.hex {
        bail!(
            "the file does not match its {} checksum (expected {}, got {actual})",
            expected.algorithm.aria2_name(),
            expected.hex
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_digest_identifies_its_own_algorithm() {
        // The six digest lengths are distinct, so no label is needed.
        for (len, expected) in [
            (32, Algorithm::Md5),
            (40, Algorithm::Sha1),
            (56, Algorithm::Sha224),
            (64, Algorithm::Sha256),
            (96, Algorithm::Sha384),
            (128, Algorithm::Sha512),
        ] {
            let digest = "a".repeat(len);
            let parsed = parse(&digest).unwrap_or_else(|| panic!("{len} hex chars should parse"));
            assert_eq!(parsed.algorithm, expected);
            assert_eq!(parsed.hex, digest);
        }
    }

    #[test]
    fn a_digest_of_the_wrong_length_is_refused() {
        // Truncated or over-long is a paste error, not a hash.
        assert_eq!(parse(&"a".repeat(63)), None);
        assert_eq!(parse(&"a".repeat(65)), None);
        assert_eq!(parse("not-hex-at-all"), None);
        assert_eq!(parse(""), None);
        // Right length, but 'z' is not hex.
        assert_eq!(parse(&format!("z{}", "a".repeat(63))), None);
    }

    #[test]
    fn labelled_forms_are_accepted() {
        let digest = "b".repeat(64);
        for text in [
            format!("sha256:{digest}"),
            format!("sha-256={digest}"),
            format!("SHA256: {digest}"),
            format!("SHA_256 = {digest}"),
        ] {
            let parsed = parse(&text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(parsed.algorithm, Algorithm::Sha256);
            assert_eq!(parsed.hex, digest);
        }
    }

    #[test]
    fn a_label_that_disagrees_with_the_length_is_refused() {
        // `sha256:` followed by an MD5 is a copy-paste accident, and taking
        // the label at its word would make aria2 fail with a confusing error.
        assert_eq!(parse(&format!("sha256:{}", "a".repeat(32))), None);
    }

    #[test]
    fn a_whole_pasted_line_works() {
        let digest = "c".repeat(64);
        // coreutils, both text and binary mode.
        let parsed = parse(&format!("{digest}  archive.tar.gz")).expect("coreutils line");
        assert_eq!(parsed.hex, digest);
        let parsed = parse(&format!("{digest} *archive.tar.gz")).expect("binary mode line");
        assert_eq!(parsed.hex, digest);
        // BSD.
        let parsed = parse(&format!("SHA256 (archive.tar.gz) = {digest}")).expect("BSD line");
        assert_eq!(parsed.algorithm, Algorithm::Sha256);
        assert_eq!(parsed.hex, digest);
    }

    #[test]
    fn the_right_line_is_picked_out_of_a_sums_file() {
        let wanted = "d".repeat(64);
        let other = "e".repeat(64);
        let body = format!(
            "# Generated by hand\n\
             {other}  other-file.iso\n\
             {wanted}  snatch-1.7.9.tar.gz\n\
             {other}  yet-another.bin\n"
        );
        let found = find_in_sums(&body, "snatch-1.7.9.tar.gz").expect("the line is found");
        assert_eq!(found.hex, wanted);
        assert_eq!(find_in_sums(&body, "absent.bin"), None);
    }

    #[test]
    fn a_sums_file_may_use_the_bsd_layout() {
        let wanted = "f".repeat(128);
        let body = format!("SHA512 (release.zip) = {wanted}\n");
        let found = find_in_sums(&body, "release.zip").expect("BSD line is found");
        assert_eq!(found.algorithm, Algorithm::Sha512);
        assert_eq!(found.hex, wanted);
    }

    #[test]
    fn a_name_with_a_directory_or_spaces_still_matches() {
        let wanted = "a".repeat(64);
        // SHA256SUMS generated one directory up keeps the path.
        let body = format!("{wanted}  ./dist/my file.tar.gz\n");
        let found = find_in_sums(&body, "my file.tar.gz").expect("matched on the basename");
        assert_eq!(found.hex, wanted);
    }

    #[test]
    fn an_exact_name_beats_a_case_insensitive_one() {
        let exact = "1".repeat(64);
        let loose = "2".repeat(64);
        let body = format!("{loose}  README.TXT\n{exact}  readme.txt\n");
        assert_eq!(find_in_sums(&body, "readme.txt").expect("found").hex, exact);
        // With no exact match, the case-insensitive one is still useful.
        let body = format!("{loose}  README.TXT\n");
        assert_eq!(find_in_sums(&body, "readme.txt").expect("found").hex, loose);
    }

    #[test]
    fn candidates_are_siblings_of_the_file() {
        let candidates = candidate_urls("https://example.com/dist/app-1.0.tar.gz");
        assert!(candidates.contains(&"https://example.com/dist/app-1.0.tar.gz.sha256".to_owned()));
        assert!(candidates.contains(&"https://example.com/dist/SHA256SUMS".to_owned()));
        // The file-specific digest is tried before the directory-wide one.
        let specific = candidates
            .iter()
            .position(|c| c.ends_with("app-1.0.tar.gz.sha256"));
        let generic = candidates.iter().position(|c| c.ends_with("SHA256SUMS"));
        assert!(specific < generic, "{candidates:?}");
        // A query string must not end up in the sibling path.
        let candidates = candidate_urls("https://example.com/d/app.bin?token=abc");
        assert!(candidates.contains(&"https://example.com/d/SHA256SUMS".to_owned()));
    }

    #[test]
    fn schemes_without_a_sibling_directory_yield_nothing() {
        assert!(candidate_urls("magnet:?xt=urn:btih:abc").is_empty());
        assert!(candidate_urls("ftp://example.com/f.bin").is_empty());
        assert!(candidate_urls("not a url").is_empty());
    }

    #[test]
    fn aria2_spelling_matches_what_aria2_accepts() {
        // Verified against `aria2c -v`: sha-1, sha-224, sha-256, sha-384,
        // sha-512, md5.
        let checksum = Checksum {
            algorithm: Algorithm::Sha256,
            hex: "a".repeat(64),
        };
        assert_eq!(
            checksum.aria2_value(),
            format!("sha-256={}", "a".repeat(64))
        );
        assert_eq!(Algorithm::Sha1.aria2_name(), "sha-1");
        assert_eq!(Algorithm::Md5.aria2_name(), "md5");
    }

    /// A one-shot HTTP server that answers from a fixed routing table.
    ///
    /// Hand-rolled on tokio's TcpListener rather than pulling in a server
    /// crate for a test: discovery is the half of this module that talks to
    /// the network, and testing it against a parser rather than a socket
    /// would not prove much.
    async fn serve(
        routes: Vec<(&'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 2048];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let response = match routes.iter().find(|(route, _)| *route == path) {
                        Some((_, body)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_owned(),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (base, handle)
    }

    #[tokio::test]
    async fn a_digest_beside_the_file_is_found() {
        // The `<file>.sha256` convention, holding the digest alone.
        const DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let (base, server) = serve(vec![("/dist/app.tar.gz.sha256", DIGEST)]).await;
        let client = reqwest::Client::new();

        let found = discover(&client, &format!("{base}/dist/app.tar.gz"), "app.tar.gz").await;
        let (checksum, source) = found.expect("the sibling digest is found");
        assert_eq!(checksum.algorithm, Algorithm::Sha256);
        assert_eq!(checksum.hex, DIGEST);
        assert!(source.ends_with("/dist/app.tar.gz.sha256"), "{source}");

        server.abort();
    }

    #[tokio::test]
    async fn a_sums_file_in_the_same_directory_is_found() {
        // No file-specific digest, so the second wave has to do the work,
        // and the right line has to be picked out of several.
        const SUMS: &str = "\
2222222222222222222222222222222222222222222222222222222222222222  other.bin
3333333333333333333333333333333333333333333333333333333333333333  app.tar.gz
";
        let (base, server) = serve(vec![("/dist/SHA256SUMS", SUMS)]).await;
        let client = reqwest::Client::new();

        let found = discover(&client, &format!("{base}/dist/app.tar.gz"), "app.tar.gz").await;
        let (checksum, source) = found.expect("the sums file is found");
        assert_eq!(checksum.hex, "3".repeat(64));
        assert!(source.ends_with("/dist/SHA256SUMS"), "{source}");

        server.abort();
    }

    #[tokio::test]
    async fn a_server_with_nothing_published_yields_nothing() {
        // Every candidate 404s. A miss must be quiet, not an error.
        let (base, server) = serve(Vec::new()).await;
        let client = reqwest::Client::new();
        let found = discover(&client, &format!("{base}/dist/app.tar.gz"), "app.tar.gz").await;
        assert!(found.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn a_sums_file_that_omits_this_file_is_not_used() {
        // A directory-wide sums file that never mentions the download must
        // not hand back somebody else's digest.
        const SUMS: &str =
            "4444444444444444444444444444444444444444444444444444444444444444  unrelated.bin\n";
        let (base, server) = serve(vec![("/dist/SHA256SUMS", SUMS)]).await;
        let client = reqwest::Client::new();
        let found = discover(&client, &format!("{base}/dist/app.tar.gz"), "app.tar.gz").await;
        assert!(found.is_none(), "{found:?}");
        server.abort();
    }

    #[tokio::test]
    async fn a_file_is_verified_against_its_real_digest() {
        let directory = std::env::temp_dir().join("snatch-checksum-verify");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch");
        let path = directory.join("payload.bin");
        std::fs::write(&path, b"snatch").expect("write");

        // sha256 of "snatch", computed independently.
        let digest = String::from_utf8(
            std::process::Command::new("sha256sum")
                .arg(&path)
                .output()
                .expect("sha256sum runs")
                .stdout,
        )
        .expect("utf8");
        let expected = parse(&digest).expect("the tool's own output parses");

        verify_file(&path, &expected).await.expect("matches");

        let wrong = Checksum {
            algorithm: Algorithm::Sha256,
            hex: "0".repeat(64),
        };
        let error = verify_file(&path, &wrong)
            .await
            .expect_err("a wrong digest must fail");
        assert!(error.to_string().contains("does not match"), "{error}");

        let _ = std::fs::remove_dir_all(&directory);
    }
}
