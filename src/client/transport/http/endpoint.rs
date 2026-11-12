//! URLs, origins, redaction and small header codecs, written by hand so the
//! transport needs no URL or encoding crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::http::uri::{Authority, PathAndQuery, Scheme};
use ::http::{HeaderValue, Uri};

use crate::error::{Error, ErrorCode, Result};

/// A parsed absolute `http`/`https` URL, without userinfo or fragment.
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// What is sent: scheme, authority (no userinfo) and path-and-query.
    pub(crate) uri: Uri,
    /// Percent-decoded `user:password` from the URL, if it had one.
    pub(crate) userinfo: Option<(Vec<u8>, Vec<u8>)>,
}

/// Scheme, host and port, compared for same-origin decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    https: bool,
    host: String,
    port: u16,
}

impl Endpoint {
    /// Parses `text` as an absolute `http://` or `https://` URL. Errors carry
    /// `code` and never echo the URL (it may hold credentials).
    pub(crate) fn parse(text: &str, code: ErrorCode, what: &str) -> Result<Self> {
        let text = text.trim();
        let text = text.split('#').next().unwrap_or(text);
        let invalid = |why: &str| Error::new(code, format!("{what} is not a valid URL: {why}"));
        let uri = Uri::try_from(text).map_err(|_| invalid("unparseable"))?;
        let scheme = match uri.scheme_str() {
            Some(s) if s.eq_ignore_ascii_case("http") => Scheme::HTTP,
            Some(s) if s.eq_ignore_ascii_case("https") => Scheme::HTTPS,
            Some(_) => return Err(invalid("the scheme must be http or https")),
            None => return Err(invalid("it is not absolute")),
        };
        let authority = uri.authority().ok_or_else(|| invalid("it has no host"))?;
        if authority.host().is_empty() {
            return Err(invalid("it has no host"));
        }
        let (userinfo, host_port) = match authority.as_str().rsplit_once('@') {
            Some((info, rest)) => {
                let (user, pass) = info.split_once(':').unwrap_or((info, ""));
                (Some((percent_decode(user), percent_decode(pass))), rest)
            }
            None => (None, authority.as_str()),
        };
        let authority = Authority::try_from(host_port).map_err(|_| invalid("bad authority"))?;
        let path = match uri.path_and_query() {
            Some(pq) if !pq.as_str().is_empty() && pq.as_str().starts_with('/') => pq.clone(),
            Some(pq) if pq.as_str().starts_with('?') => {
                PathAndQuery::try_from(format!("/{}", pq.as_str()))
                    .map_err(|_| invalid("bad path"))?
            }
            _ => PathAndQuery::from_static("/"),
        };
        let uri = Uri::builder()
            .scheme(scheme)
            .authority(authority)
            .path_and_query(path)
            .build()
            .map_err(|_| invalid("unparseable"))?;
        Ok(Endpoint { uri, userinfo })
    }
}

/// The origin of an absolute URI.
pub(crate) fn origin(uri: &Uri) -> Origin {
    let https = is_https(uri);
    Origin {
        https,
        host: uri.host().unwrap_or("").to_ascii_lowercase(),
        port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
    }
}

/// Whether `uri` is `https`.
pub(crate) fn is_https(uri: &Uri) -> bool {
    uri.scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
}

/// The URL for `Debug` and error messages: userinfo, query and fragment are
/// replaced, so credentials and tokens in the URL never leak.
pub(crate) fn redact_url(text: &str) -> String {
    let text = text.trim();
    let mut out = String::with_capacity(text.len() + 16);
    let rest = match text.split_once("://") {
        Some((scheme, rest)) => {
            out.push_str(scheme);
            out.push_str("://");
            rest
        }
        None => text,
    };
    // The last `@` ends the userinfo wherever the authority seems to end, so
    // a password holding `/`, `?` or `#` is redacted too (the URL may be
    // malformed; err toward over-redaction).
    // An `@` after the first `?`/`#` cannot be told from a `?` or `#` inside
    // a password, so nothing past the scheme is printed.
    if let (Some(at), Some(cut)) = (rest.rfind('@'), rest.find(['?', '#'])) {
        if at > cut {
            out.push_str("<redacted>");
            return out;
        }
    }
    let rest = match rest.rsplit_once('@') {
        Some((_, after)) => {
            out.push_str("<redacted>@");
            after
        }
        None => rest,
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((t, _)) => (t, true),
        None => (rest, false),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((t, _)) => (t, true),
        None => (rest, false),
    };
    out.push_str(rest);
    if query {
        out.push_str("?<redacted>");
    }
    if fragment {
        out.push_str("#<redacted>");
    }
    out
}

/// Resolves a `Location` header against `base` (RFC 3986 §5.2, with the
/// dot segments of the merged path removed, §5.2.4). `None` when the value
/// is not a usable reference, including an absolute URI whose scheme is not
/// `http` or `https`.
pub(crate) fn resolve_location(base: &Uri, location: &str) -> Option<Uri> {
    let location = location.trim();
    let location = location.split('#').next().unwrap_or(location);
    if location.is_empty() {
        return None;
    }
    let lower = location.get(..8).unwrap_or(location).to_ascii_lowercase();
    let absolute = if lower.starts_with("http://") || lower.starts_with("https://") {
        location.to_owned()
    } else if has_scheme(location) {
        // `ftp://…`, `mailto:…` or `http:x` (no authority): not a target to
        // merge with the base path (RFC 3986 §4.3).
        return None;
    } else {
        let scheme = base.scheme_str()?;
        let authority = base.authority()?.as_str();
        if let Some(rest) = location.strip_prefix("//") {
            format!("{scheme}://{rest}")
        } else if location.starts_with('/') {
            format!("{scheme}://{authority}{location}")
        } else if location.starts_with('?') {
            format!("{scheme}://{authority}{}{location}", base.path())
        } else {
            let path = base.path();
            let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
            let dir = if dir.is_empty() { "/" } else { dir };
            format!("{scheme}://{authority}{dir}{location}")
        }
    };
    let absolute = without_dot_segments(&absolute);
    Endpoint::parse(&absolute, ErrorCode::HttpStatus, "the redirect target")
        .ok()
        .filter(|e| e.userinfo.is_none())
        .map(|e| e.uri)
}

/// Whether a reference starts with `scheme ":"` (RFC 3986 §3.1: ALPHA
/// followed by ALPHA, DIGIT, `+`, `-` or `.`), before any `/`, `?` or `#`.
fn has_scheme(reference: &str) -> bool {
    let Some(colon) = reference.find([':', '/', '?', '#']) else {
        return false;
    };
    let scheme = reference.as_bytes();
    scheme[colon] == b':'
        && colon > 0
        && scheme[0].is_ascii_alphabetic()
        && scheme[1..colon]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// `scheme://authority/path?query` with `remove_dot_segments` (RFC 3986
/// §5.2.4) applied to the path; the query is kept as it is.
fn without_dot_segments(uri: &str) -> String {
    let Some(start) = uri.find("://").map(|i| i + 3) else {
        return uri.to_owned();
    };
    let Some(path_start) = uri[start..].find(['/', '?']).map(|i| start + i) else {
        return uri.to_owned();
    };
    let path_end = uri[path_start..]
        .find('?')
        .map_or(uri.len(), |i| path_start + i);
    let path = &uri[path_start..path_end];
    if !path.contains('.') {
        return uri.to_owned();
    }
    let mut out = String::with_capacity(uri.len());
    out.push_str(&uri[..path_start]);
    out.push_str(&remove_dot_segments(path));
    out.push_str(&uri[path_end..]);
    out
}

/// RFC 3986 §5.2.4 `remove_dot_segments`.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            output.truncate(output.rfind('/').unwrap_or(0));
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let skip = usize::from(input.starts_with('/'));
            let end = input[skip..].find('/').map_or(input.len(), |i| skip + i);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

/// Decodes `%XX` escapes to the octets they name (not necessarily UTF-8);
/// invalid escapes are kept literally.
pub(crate) fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Standard base64 with padding (RFC 4648 §4).
pub(crate) fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for group in input.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (k, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if k <= group.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An `Authorization`/`Proxy-Authorization` value for HTTP Basic, marked
/// sensitive.
pub(crate) fn basic_auth(user: &[u8], password: &[u8]) -> Option<HeaderValue> {
    let mut credentials = Vec::with_capacity(user.len() + 1 + password.len());
    credentials.extend_from_slice(user);
    credentials.push(b':');
    credentials.extend_from_slice(password);
    let mut value = HeaderValue::try_from(format!("Basic {}", base64(&credentials))).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// Longest `Retry-After` reported: one day. A peer's larger value (up to
/// `u64::MAX` seconds) is clamped, so nobody adding it to an `Instant`
/// overflows.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(86_400);

/// Reads a `Retry-After` value: delay-seconds or an HTTP-date in
/// any form `parse_http_date` reads, clamped to [`MAX_RETRY_AFTER`]. A date
/// in the past is zero.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    let secs = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // All digits but too many for u64: still a (very long) delay.
        value.parse::<u64>().unwrap_or(u64::MAX)
    } else {
        let at = parse_http_date(value)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        at.saturating_sub(now)
    };
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// Seconds since the Unix epoch for an HTTP-date in any of the three formats
/// RFC 9110 §5.6.7 requires a recipient to accept: IMF-fixdate
/// (`Sun, 06 Nov 1994 08:49:37 GMT`), RFC 850 (`Sunday, 06-Nov-94 08:49:37
/// GMT`) and asctime (`Sun Nov  6 08:49:37 1994`), with a year in
/// 1970..=9999 (anything else is not a date this reads). Checked throughout.
fn parse_http_date(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split_ascii_whitespace().take(7).collect();
    match parts.as_slice() {
        // IMF-fixdate.
        [weekday, day, month, year, clock, "GMT"] if weekday.ends_with(',') => civil_seconds(
            day.parse().ok()?,
            month_number(month)?,
            year.parse().ok()?,
            clock,
        ),
        // RFC 850: a two-digit year, read as the nearest to now (a date
        // more than 50 years ahead is in the previous century).
        [weekday, date, clock, "GMT"] if weekday.ends_with(',') => {
            let mut date = date.split('-');
            let day = date.next()?.parse().ok()?;
            let month = month_number(date.next()?)?;
            let year = date.next()?;
            if date.next().is_some() || year.len() != 2 {
                return None;
            }
            let yy: i64 = year.parse().ok()?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            let this_century = civil_seconds(day, month, 2000 + yy, clock)?;
            if this_century > now.saturating_add(50 * 365 * 86_400 + 12 * 86_400) {
                civil_seconds(day, month, 1900 + yy, clock)
            } else {
                Some(this_century)
            }
        }
        // asctime.
        [weekday, month, day, clock, year] if !weekday.ends_with(',') => civil_seconds(
            day.parse().ok()?,
            month_number(month)?,
            year.parse().ok()?,
            clock,
        ),
        _ => None,
    }
}

fn month_number(name: &str) -> Option<i64> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Seconds since the epoch for a civil date and an `hh:mm:ss` clock.
fn civil_seconds(day: u64, month: i64, year: i64, clock: &str) -> Option<u64> {
    let mut clock = clock.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || !(1970..=9999).contains(&year)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from 1970-01-01 to the civil date (proleptic Gregorian).
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let doy = (153 * m + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era
        .checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)?;
    let days = u64::try_from(days).ok()?;
    days.checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_strips_credentials() {
        let e = Endpoint::parse(
            "HTTPS://u%40x:p%3Aw@Example.com:8443/run?k=v#frag",
            ErrorCode::InvalidConfig,
            "url",
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(e.uri.to_string(), "https://Example.com:8443/run?k=v");
        assert_eq!(e.userinfo, Some((b"u@x".to_vec(), b"p:w".to_vec())));
        assert_eq!(
            origin(&e.uri),
            origin(&"https://example.com:8443/x".parse().unwrap_or_default())
        );
        for bad in ["ftp://x/", "/relative", "http://", "not a url"] {
            assert!(
                Endpoint::parse(bad, ErrorCode::InvalidConfig, "url").is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            Endpoint::parse("http://h", ErrorCode::InvalidConfig, "url")
                .map(|e| e.uri.to_string())
                .ok(),
            Some("http://h/".to_owned())
        );
    }

    #[test]
    fn redaction() {
        assert_eq!(
            redact_url("https://user:pw@h:1/p?token=s#f"),
            "https://<redacted>@h:1/p?<redacted>#<redacted>"
        );
        assert_eq!(redact_url("http://h/p"), "http://h/p");
    }

    /// A password holding `/`, `?` or `#` (not percent-encoded) is still
    /// redacted: the last `@` ends the userinfo, wherever the authority
    /// seems to end.
    #[test]
    fn redaction_survives_delimiters_in_the_password() {
        assert_eq!(
            redact_url("http://svc:pa/ss@host/run"),
            "http://<redacted>@host/run"
        );
        for url in ["http://svc:pa?ss@host/run", "http://svc:pa#ss@host/"] {
            let out = redact_url(url);
            assert!(!out.contains("pa") && !out.contains("ss"), "{out}");
        }
        for url in [
            "http://host/run?email=a@b.com&token=SECRET",
            "http://host/p#x@y&k=SECRET",
        ] {
            let out = redact_url(url);
            assert!(!out.contains("SECRET"), "{out}");
        }
    }

    #[test]
    fn obsolete_http_date_forms_are_read() {
        let expected = Some(784_111_777);
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), expected);
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), expected);
        // Two-digit years: within 50 years of now is this century.
        assert_eq!(
            parse_http_date("Thursday, 01-Jan-99 00:00:00 GMT"),
            Some(915_148_800)
        );
        assert_eq!(
            parse_http_date("Tuesday, 01-Jan-30 00:00:00 GMT"),
            Some(1_893_456_000)
        );
        for bad in [
            "Sunday, 06-Nov-1994 08:49:37 GMT",
            "Sun Nov 6 08:49:37 1994 GMT",
            "Sunday, 06-Foo-94 08:49:37 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn locations() {
        let base: Uri = "http://h:1/a/b?q".parse().unwrap_or_default();
        let r = |l: &str| resolve_location(&base, l).map(|u| u.to_string());
        assert_eq!(r("/x").as_deref(), Some("http://h:1/x"));
        assert_eq!(r("c").as_deref(), Some("http://h:1/a/c"));
        assert_eq!(r("?z").as_deref(), Some("http://h:1/a/b?z"));
        assert_eq!(r("//o:2/y").as_deref(), Some("http://o:2/y"));
        assert_eq!(r("https://o/y#f").as_deref(), Some("https://o/y"));
        assert_eq!(r("http://u:p@o/"), None);
        assert_eq!(r(""), None);
    }

    /// Dot segments are removed from the merged path (RFC 3986 §5.2.2,
    /// §5.2.4): the result is the next request target.
    #[test]
    fn locations_remove_dot_segments() {
        let base: Uri = "https://agent/api/v1/run".parse().unwrap_or_default();
        let r = |l: &str| resolve_location(&base, l).map(|u| u.to_string());
        assert_eq!(r("../v2/run").as_deref(), Some("https://agent/api/v2/run"));
        assert_eq!(r("./x").as_deref(), Some("https://agent/api/v1/x"));
        assert_eq!(r("/a/./b/../c").as_deref(), Some("https://agent/a/c"));
        assert_eq!(
            r("../../../../x?a/../b").as_deref(),
            Some("https://agent/x?a/../b")
        );
        assert_eq!(r("https://o/a/../b").as_deref(), Some("https://o/b"));
        assert_eq!(r("..").as_deref(), Some("https://agent/api/"));
    }

    /// A reference with a scheme other than http/https is an absolute URI
    /// (RFC 3986 §4.3), not a relative path to merge.
    #[test]
    fn locations_with_other_schemes_are_refused() {
        let base: Uri = "https://agent/api/run".parse().unwrap_or_default();
        let r = |l: &str| resolve_location(&base, l).map(|u| u.to_string());
        assert_eq!(r("ftp://other/x"), None);
        assert_eq!(r("mailto:a@b"), None);
        assert_eq!(r("javascript:x"), None);
        assert_eq!(r("./a:b").as_deref(), Some("https://agent/api/a:b"));
        assert_eq!(r("x/a:b").as_deref(), Some("https://agent/api/x/a:b"));
    }

    #[test]
    fn codecs() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(percent_decode("a%20b%zz%4"), b"a b%zz%4");
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after("-1"), None);
    }

    /// Security audit 2, L1: a hostile `Retry-After` neither overflows (a
    /// debug-build panic, a wrapped value in release) nor reports an
    /// unbounded delay.
    #[test]
    fn hostile_retry_after_is_bounded_and_never_overflows() {
        for year in [
            "1000000000000",
            "9223372036854775807",
            "-5",
            "1969",
            "10000",
        ] {
            let date = format!("Sun, 06 Nov {year} 08:49:37 GMT");
            assert_eq!(parse_http_date(&date), None, "{date}");
            assert_eq!(parse_retry_after(&date), None, "{date}");
        }
        assert_eq!(
            parse_http_date("Fri, 31 Dec 9999 23:59:59 GMT"),
            Some(253_402_300_799)
        );
        assert_eq!(
            parse_retry_after("Fri, 31 Dec 9999 23:59:59 GMT"),
            Some(MAX_RETRY_AFTER)
        );
        for secs in ["18446744073709551615", "99999999999999999999999", "86401"] {
            assert_eq!(parse_retry_after(secs), Some(MAX_RETRY_AFTER), "{secs}");
        }
        assert_eq!(
            parse_retry_after("86400"),
            Some(Duration::from_secs(86_400))
        );
    }
}
