//! Parsing a browser's "Copy as cURL" command into a download request.
//!
//! Every browser's network inspector can copy a request as a `curl`
//! invocation. That one string carries the URL, the cookies, the referer, the
//! user agent and whatever bearer token or signed header the site required —
//! which is precisely the set of things that make a download behind a login
//! work. Pasting it beats re-deriving any of that by hand.
//!
//! Three quoting dialects have to be handled, because the browsers disagree:
//!
//! * Chrome and Firefox on Linux/macOS emit POSIX single quotes, escaping an
//!   embedded quote as `'\''`.
//! * "Copy as cURL (cmd)" on Windows emits double quotes with `\"` escapes and
//!   `^` line continuations.
//! * "Copy as cURL (bash)" wraps lines with a trailing backslash.

use anyhow::{Result, bail};

use crate::types::DownloadRequest;

/// Turn a `curl ...` command line into a request.
pub fn parse(command: &str) -> Result<DownloadRequest> {
    let tokens = tokenise(command);
    if tokens.is_empty() {
        bail!("nothing to parse");
    }
    if !tokens[0].trim_end_matches(".exe").ends_with("curl") {
        bail!("that does not look like a curl command");
    }

    let mut url: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut cookies: Option<String> = None;
    let mut user_agent: Option<String> = None;
    let mut referer: Option<String> = None;
    let mut username: Option<String> = None;
    let mut password: Option<String> = None;

    let mut index = 1;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        // `--header value` and `--header=value` are both valid.
        let (flag, inline) = match token.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (token, None),
        };

        let take = |index: &mut usize| -> Option<String> {
            if let Some(value) = inline.clone() {
                return Some(value);
            }
            *index += 1;
            tokens.get(*index).cloned()
        };

        match flag {
            "-H" | "--header" => {
                if let Some(value) = take(&mut index)
                    && let Some((name, body)) = value.split_once(':')
                {
                    headers.push((name.trim().to_owned(), body.trim().to_owned()));
                }
            }
            "-b" | "--cookie" => {
                if let Some(value) = take(&mut index) {
                    cookies = Some(value);
                }
            }
            "-A" | "--user-agent" => {
                if let Some(value) = take(&mut index) {
                    user_agent = Some(value);
                }
            }
            "-e" | "--referer" => {
                if let Some(value) = take(&mut index) {
                    referer = Some(value);
                }
            }
            "--url" => {
                if let Some(value) = take(&mut index) {
                    url = Some(value);
                }
            }
            // `-u user:password`. The password may itself contain a colon, so
            // only the first one separates the two.
            "-u" | "--user" => {
                if let Some(value) = take(&mut index) {
                    let (name, secret) = value
                        .split_once(':')
                        .map_or((value.as_str(), ""), |(n, s)| (n, s));
                    if !name.is_empty() {
                        username = Some(name.to_owned());
                        password = Some(secret.to_owned());
                    }
                }
            }
            // Flags that take a value we do not need, but whose argument must
            // not be mistaken for the URL.
            "-X" | "--request" | "-d" | "--data" | "--data-raw" | "--data-binary"
            | "--data-urlencode" | "-o" | "--output" | "--proxy" | "-x" | "--connect-timeout"
            | "--max-time" | "-m" | "--retry" | "-w" | "--write-out" | "-T" | "--upload-file"
            | "-F" | "--form" | "--cookie-jar" | "-c" => {
                let _ = take(&mut index);
            }
            other if other.starts_with('-') => {
                // A bare switch such as --compressed, -L, -k, -s.
            }
            other => {
                // The first bare word is the URL. Later ones are ignored: a
                // browser only ever emits one.
                if url.is_none() && !other.is_empty() {
                    url = Some(other.to_owned());
                }
            }
        }
        index += 1;
    }

    let url = url.unwrap_or_default();
    if url.is_empty() {
        bail!("the command has no URL");
    }

    // Headers carry the interesting parts, and they win over the dedicated
    // flags because a browser emits them that way.
    for (name, value) in &headers {
        match name.to_ascii_lowercase().as_str() {
            "cookie" => cookies = Some(value.clone()),
            "user-agent" => user_agent = Some(value.clone()),
            "referer" | "referrer" => referer = Some(value.clone()),
            _ => {}
        }
    }

    let mut request = DownloadRequest::from_url(url);
    request.cookies = cookies.filter(|value| !value.trim().is_empty());
    request.user_agent = user_agent.filter(|value| !value.trim().is_empty());
    request.referer = referer.filter(|value| !value.trim().is_empty());
    request.username = username.filter(|value| !value.trim().is_empty());
    request.password = password;
    request.validate()?;
    Ok(request)
}

/// Does this look like a cURL command rather than a bare URL?
pub fn looks_like_curl(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("curl ") || trimmed.starts_with("curl.exe ")
}

/// Split a shell-ish command line into tokens.
///
/// Not a shell: no expansion, no substitution, no operators. Just enough
/// quoting to survive what the browsers actually emit.
fn tokenise(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = command.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            // Line continuations, in both the bash and cmd dialects.
            '\\' | '^' if matches!(chars.peek(), Some('\n') | Some('\r')) => {
                while matches!(chars.peek(), Some('\n') | Some('\r')) {
                    chars.next();
                }
            }
            '\\' if matches!(chars.peek(), Some('"')) => {
                // `\"` inside an unquoted run.
                current.push('"');
                started = true;
                chars.next();
            }
            '\'' => {
                started = true;
                // Inside single quotes everything is literal. Chrome escapes
                // an embedded quote by closing, emitting \' and reopening.
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                started = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(escaped @ ('"' | '\\' | '$' | '`')) => current.push(escaped),
                            Some('n') => current.push('\n'),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            character if character.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            other => {
                current.push(other);
                started = true;
            }
        }
    }

    if started {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_curl_command() {
        assert!(looks_like_curl("curl 'https://x/'"));
        assert!(looks_like_curl("  curl.exe \"https://x/\""));
        assert!(!looks_like_curl("https://example.com/file.iso"));
        assert!(!looks_like_curl("wget https://x/"));
    }

    #[test]
    fn parses_the_chrome_linux_dialect() {
        // Single quotes, one -H per header: what Chrome and Firefox emit.
        let command = r#"curl 'https://cdn.example.com/private/file.zip' \
  -H 'authority: cdn.example.com' \
  -H 'accept: */*' \
  -H 'cookie: session=abc123; theme=dark' \
  -H 'referer: https://example.com/downloads' \
  -H 'user-agent: Mozilla/5.0 (X11; Linux x86_64)' \
  --compressed"#;

        let request = parse(command).expect("a Chrome cURL parses");
        assert_eq!(request.url, "https://cdn.example.com/private/file.zip");
        assert_eq!(
            request.cookies.as_deref(),
            Some("session=abc123; theme=dark")
        );
        assert_eq!(
            request.referer.as_deref(),
            Some("https://example.com/downloads")
        );
        assert_eq!(
            request.user_agent.as_deref(),
            Some("Mozilla/5.0 (X11; Linux x86_64)")
        );
    }

    #[test]
    fn parses_the_windows_cmd_dialect() {
        // Double quotes, ^ continuations.
        let command = "curl \"https://x.test/a.bin\" ^\n  -H \"Cookie: a=1\" ^\n  -H \"User-Agent: Edge/1.0\"";
        let request = parse(command).expect("a cmd cURL parses");
        assert_eq!(request.url, "https://x.test/a.bin");
        assert_eq!(request.cookies.as_deref(), Some("a=1"));
        assert_eq!(request.user_agent.as_deref(), Some("Edge/1.0"));
    }

    #[test]
    fn dedicated_flags_work_too() {
        let request =
            parse("curl -b 'k=v' -A 'Agent/2' -e 'https://ref.example' https://x.test/f.bin")
                .expect("flag form parses");
        assert_eq!(request.url, "https://x.test/f.bin");
        assert_eq!(request.cookies.as_deref(), Some("k=v"));
        assert_eq!(request.user_agent.as_deref(), Some("Agent/2"));
        assert_eq!(request.referer.as_deref(), Some("https://ref.example"));
    }

    #[test]
    fn a_flag_argument_is_never_mistaken_for_the_url() {
        // -X POST would otherwise leave "POST" looking like a bare word, and
        // -o would capture the output filename as the URL.
        let request =
            parse("curl -X POST -o out.bin --retry 3 https://x.test/real").expect("parses");
        assert_eq!(request.url, "https://x.test/real");
    }

    #[test]
    fn headers_beat_the_equivalent_flags() {
        // A browser emits both forms only rarely, but the header is the one
        // that actually went over the wire.
        let request = parse("curl -A 'Old' -H 'user-agent: New' https://x.test/f").expect("parses");
        assert_eq!(request.user_agent.as_deref(), Some("New"));
    }

    #[test]
    fn handles_an_escaped_quote_inside_a_value() {
        let request = parse(r#"curl "https://x.test/f" -H "X-Note: say \"hi\"""#).expect("parses");
        assert_eq!(request.url, "https://x.test/f");
    }

    #[test]
    fn rejects_what_is_not_a_curl_command() {
        assert!(parse("wget https://x/").is_err());
        assert!(parse("").is_err());
        // No URL at all.
        assert!(parse("curl -H 'a: b'").is_err());
        // A scheme no engine can fetch.
        assert!(parse("curl 'file:///etc/passwd'").is_err());
    }

    #[test]
    fn credentials_come_from_the_user_flag() {
        let request = parse("curl -u alice:s3cret https://x.test/private.bin").expect("parses");
        assert_eq!(request.url, "https://x.test/private.bin");
        assert_eq!(request.username.as_deref(), Some("alice"));
        assert_eq!(request.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn a_password_may_contain_a_colon() {
        // Only the first colon separates the pair, or a password like a URL
        // would be truncated at its scheme.
        let request = parse("curl --user 'bob:pa:ss:word' https://x.test/f").expect("parses");
        assert_eq!(request.username.as_deref(), Some("bob"));
        assert_eq!(request.password.as_deref(), Some("pa:ss:word"));
    }

    #[test]
    fn a_username_with_no_password_is_accepted() {
        // Anonymous FTP and a few HTTP endpoints take an empty password.
        let request = parse("curl -u anonymous ftp://x.test/pub/f").expect("parses");
        assert_eq!(request.username.as_deref(), Some("anonymous"));
        assert_eq!(request.password.as_deref(), Some(""));
        assert_eq!(
            request.credentials(),
            Some(("anonymous".to_owned(), String::new()))
        );
    }

    #[test]
    fn the_user_flag_no_longer_swallows_the_url() {
        // -u used to be discarded along with its argument; the URL still has
        // to survive that.
        let request = parse("curl -u a:b -X POST https://x.test/real").expect("parses");
        assert_eq!(request.url, "https://x.test/real");
    }

    #[test]
    fn the_url_flag_is_honoured() {
        let request = parse("curl --url 'https://x.test/via-flag'").expect("parses");
        assert_eq!(request.url, "https://x.test/via-flag");
    }
}
