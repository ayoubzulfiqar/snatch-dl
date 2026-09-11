//! What the Browse page refuses to load.
//!
//! Streaming pages -- IPTV, sports, the long tail of sites that carry a live
//! channel -- pay for themselves with popunders: an ad network script that
//! hijacks the first click anywhere on the page and opens a new window behind
//! it. Blocking the *window* is half the answer and lives in `browser.rs`.
//! This is the other half: not loading the script that does it.
//!
//! It is WebKit's own content blocker, the same one Safari uses. Rules are
//! compiled once into a matcher that runs inside the network layer, before a
//! request is made, so a blocked ad is never fetched at all -- faster than
//! anything that inspects responses, and nothing a page's script can undo.
//!
//! The list is short on purpose. It names ad and popunder *networks* -- the
//! companies whose only business is serving ads -- and nothing that also
//! serves content. EasyList is far more thorough and far larger, and much of
//! its syntax has no equivalent in WebKit's rule format; a small list that is
//! certainly right beats a large one that might block the video.

/// Networks that exist to serve popunders, redirects and interstitials.
///
/// These are the ones that matter most here. A page that loads any of them
/// is going to open a window the reader did not ask for.
const POPUNDER_NETWORKS: &[&str] = &[
    "propellerads.com",
    "propellerclick.com",
    "monetag.com",
    "popads.net",
    "popcash.net",
    "popmyads.com",
    "popunder.net",
    "adsterra.com",
    "adsterratools.com",
    "hilltopads.net",
    "hilltopads.com",
    "clickadu.com",
    "exoclick.com",
    "exosrv.com",
    "juicyads.com",
    "trafficjunky.com",
    "trafficjunky.net",
    "trafficstars.com",
    "tsyndicate.com",
    "adcash.com",
    "onclickads.net",
    "onclkds.com",
    "richads.com",
    "galaksion.com",
    "admaven.com",
    "ad-maven.com",
    "pushground.com",
    "evadav.com",
    "rollerads.com",
    "a-ads.com",
    "adxpansion.com",
    "realsrv.com",
    "magsrv.com",
    "zeydoo.com",
    "clickaine.com",
    "bidvertiser.com",
];

/// Display advertising and the tracking that funds it.
const AD_NETWORKS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.com",
    "amazon-adsystem.com",
    "taboola.com",
    "outbrain.com",
    "criteo.com",
    "criteo.net",
    "adnxs.com",
    "pubmatic.com",
    "rubiconproject.com",
    "openx.net",
    "casalemedia.com",
    "moatads.com",
    "scorecardresearch.com",
    "quantserve.com",
    "adform.net",
    "smartadserver.com",
    "media.net",
    "revcontent.com",
    "mgid.com",
    "zedo.com",
    "33across.com",
    "teads.tv",
    "sharethrough.com",
    "yieldmo.com",
];

/// Every blocked domain.
pub fn blocked_domains() -> impl Iterator<Item = &'static str> {
    POPUNDER_NETWORKS.iter().chain(AD_NETWORKS).copied()
}

/// Escape a domain for use inside a WebKit `url-filter` regular expression.
///
/// WebKit's rule language is a restricted regex, and a dot left unescaped
/// matches any character -- which would make `media.net` also block
/// `mediaXnet`, and in principle something that is not an ad at all.
fn escape(domain: &str) -> String {
    let mut escaped = String::with_capacity(domain.len() + 4);
    for character in domain.chars() {
        if matches!(
            character,
            '.' | '-' | '+' | '?' | '*' | '(' | ')' | '[' | ']'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The rule list, in the JSON WebKit's content blocker compiles.
///
/// One rule per domain, matching that domain and every subdomain of it but
/// nothing that merely contains it: `^[^:]+://+([^:/]+\.)?doubleclick\.net[:/]`
/// blocks `ad.doubleclick.net/...` and leaves `notdoubleclick.net` alone.
pub fn rules_json() -> String {
    let rules: Vec<serde_json::Value> = blocked_domains()
        .map(|domain| {
            serde_json::json!({
                "trigger": {
                    "url-filter": format!("^[^:]+://+([^:/]+\\.)?{}[:/]", escape(domain)),
                },
                "action": { "type": "block" },
            })
        })
        .collect();
    serde_json::Value::Array(rules).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the rules exactly as written against an address.
    ///
    /// The patterns come out of `rules_json` itself rather than being rebuilt
    /// here, so what is tested is the text WebKit is actually handed. WebKit
    /// compiles a subset of JavaScript regular expressions; these use only
    /// anchors, a group, a character class and escaped literals, which mean
    /// the same thing in the `regex` crate.
    fn matched(url: &str) -> bool {
        let parsed: serde_json::Value = serde_json::from_str(&rules_json()).expect("valid JSON");
        parsed.as_array().expect("a list").iter().any(|rule| {
            let filter = rule["trigger"]["url-filter"]
                .as_str()
                .expect("a url-filter");
            regex::Regex::new(filter)
                .unwrap_or_else(|error| panic!("{filter} is not a valid pattern: {error}"))
                .is_match(url)
        })
    }

    #[test]
    fn the_rules_are_json_webkit_will_compile() {
        let parsed: serde_json::Value =
            serde_json::from_str(&rules_json()).expect("the rule list is valid JSON");
        let rules = parsed.as_array().expect("a list of rules");
        assert_eq!(rules.len(), blocked_domains().count());
        for rule in rules {
            assert_eq!(rule["action"]["type"], "block");
            let filter = rule["trigger"]["url-filter"]
                .as_str()
                .expect("a url-filter");
            assert!(filter.starts_with("^[^:]+://+"), "{filter}");
            // No bare dot survives into a rule: every one is escaped.
            let body = &filter["^[^:]+://+([^:/]+\\.)?".len()..];
            assert!(!body.contains('.') || body.contains("\\."), "{filter}");
        }
    }

    #[test]
    fn a_popunder_network_and_its_subdomains_are_blocked() {
        assert!(matched("https://propellerads.com/tag.js"));
        assert!(matched("https://cdn.propellerads.com/x.js"));
        assert!(matched("https://ad.doubleclick.net/ddm/ad/123"));
        assert!(matched(
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js"
        ));
    }

    /// The whole point is to block ads and never the video.
    #[test]
    fn nothing_that_serves_the_video_is_blocked() {
        for url in [
            "https://rr1---sn-abc.googlevideo.com/videoplayback?id=1",
            "https://www.youtube.com/watch?v=abc",
            "https://i.ytimg.com/vi/abc/hq.jpg",
            "https://www.gstatic.com/x.js",
            "https://d2zihajmogu5jn.cloudfront.net/live/master.m3u8",
            "https://cph-p2p-msl.akamaized.net/hls/live/master.m3u8",
            "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
            "https://www.google.com/search?q=x",
            // Contains a blocked name without being that domain.
            "https://notdoubleclick.net/",
            "https://mymedia.network/",
        ] {
            assert!(!matched(url), "{url} would be blocked");
        }
    }

    #[test]
    fn a_dot_is_escaped_so_it_matches_only_a_dot() {
        assert_eq!(escape("media.net"), "media\\.net");
        assert_eq!(escape("ad-maven.com"), "ad\\-maven\\.com");
    }

    #[test]
    fn no_domain_is_listed_twice() {
        let mut seen = std::collections::HashSet::new();
        for domain in blocked_domains() {
            assert!(seen.insert(domain), "{domain} is listed twice");
        }
    }
}
