//! A small cookie jar with the rules of aiohttp's `CookieJar(unsafe=True)`.
//!
//! RFC 6265 storage and matching: host-only and `Domain` cookies (IP hosts
//! allowed, as `unsafe=True` does), default and explicit `Path`, `Secure`
//! cookies sent only over HTTPS, and expiry by `Max-Age` (which wins) or
//! `Expires`. That covers everything a single-host API sets.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::Uri;

#[derive(Debug, Clone)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    expires: Option<SystemTime>,
}

impl Cookie {
    fn expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|expires| expires <= now)
    }

    fn same_slot(&self, other: &Cookie) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }

    fn matches(&self, host: &str, path: &str, secure: bool) -> bool {
        let domain_ok = if self.host_only {
            host == self.domain
        } else {
            domain_matches(host, &self.domain)
        };
        domain_ok && path_matches(path, &self.path) && (secure || !self.secure)
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            && host.parse::<std::net::IpAddr>().is_err()
            && domain.parse::<std::net::IpAddr>().is_err())
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

/// RFC 6265's default path: the request path up to its last `/`.
fn default_path(request_path: &str) -> String {
    match request_path.rfind('/') {
        Some(index) if index > 0 && request_path.starts_with('/') => {
            request_path[..index].to_owned()
        }
        _ => "/".to_owned(),
    }
}

fn parse_expires(text: &str) -> Option<SystemTime> {
    // Cookie dates often use dashes ("01-Jan-1970"), which HTTP dates do not.
    httpdate::parse_http_date(text)
        .or_else(|_| httpdate::parse_http_date(&text.replace('-', " ")))
        .ok()
}

fn host_of(uri: &Uri) -> String {
    uri.host()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// The cookies one environment has been given.
#[derive(Debug, Default)]
pub(crate) struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Stores the cookie of one `Set-Cookie` header received from `uri`.
    pub(crate) fn store(&mut self, uri: &Uri, header: &str, now: SystemTime) {
        let mut parts = header.split(';');
        let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
            return;
        };
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let host = host_of(uri);
        let mut cookie = Cookie {
            name: name.to_owned(),
            value: value.trim().trim_matches('"').to_owned(),
            domain: host.clone(),
            host_only: true,
            path: default_path(uri.path()),
            secure: false,
            expires: None,
        };
        let mut max_age = None;
        let mut expires = None;
        for attribute in parts {
            let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "domain" if !value.is_empty() => {
                    let domain = value.trim_start_matches('.').to_ascii_lowercase();
                    if !domain_matches(&host, &domain) {
                        return;
                    }
                    cookie.domain = domain;
                    cookie.host_only = false;
                }
                "path" if value.starts_with('/') => cookie.path = value.to_owned(),
                "secure" => cookie.secure = true,
                "max-age" => max_age = value.parse::<i64>().ok(),
                "expires" => expires = parse_expires(value),
                _ => {}
            }
        }
        cookie.expires = match max_age {
            Some(seconds) if seconds <= 0 => Some(UNIX_EPOCH),
            Some(seconds) => Some(
                now.checked_add(Duration::from_secs(seconds as u64))
                    .unwrap_or(now + Duration::from_secs(u32::MAX.into())),
            ),
            None => expires,
        };
        match self.cookies.iter_mut().find(|old| old.same_slot(&cookie)) {
            Some(old) => *old = cookie,
            None => self.cookies.push(cookie),
        }
        self.cookies.retain(|cookie| !cookie.expired(now));
    }

    /// The `Cookie` header for a request to `uri`, if any cookie applies.
    /// Longer paths come first, then older cookies, as RFC 6265 asks.
    pub(crate) fn header(&mut self, uri: &Uri, now: SystemTime) -> Option<String> {
        self.cookies.retain(|cookie| !cookie.expired(now));
        let host = host_of(uri);
        let secure = uri.scheme_str() == Some("https");
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.matches(&host, uri.path(), secure))
            .collect();
        if matching.is_empty() {
            return None;
        }
        matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        let pairs: Vec<String> = matching
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        Some(pairs.join("; "))
    }

    /// Every live cookie as `(name, value)`.
    pub(crate) fn pairs(&self, now: SystemTime) -> Vec<(String, String)> {
        self.cookies
            .iter()
            .filter(|cookie| !cookie.expired(now))
            .map(|cookie| (cookie.name.clone(), cookie.value.clone()))
            .collect()
    }

    /// Forgets every cookie.
    pub(crate) fn clear(&mut self) {
        self.cookies.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(text: &str) -> Uri {
        text.parse().unwrap()
    }

    #[test]
    fn host_only_cookies_and_paths() {
        let now = SystemTime::now();
        let mut jar = CookieJar::default();
        let api = uri("https://api.example.com/app/account/login");
        jar.store(&api, "a=1", now);
        jar.store(&api, "b=\"2\"; Path=/; Secure; HttpOnly", now);
        jar.store(&api, "c=3; Path=/other", now);
        jar.store(&api, "=bad", now);
        jar.store(&api, "nothing", now);
        // The default path is the directory of the request.
        assert_eq!(
            jar.header(&uri("https://api.example.com/app/account/x"), now)
                .unwrap(),
            "a=1; b=2"
        );
        assert_eq!(
            jar.header(&uri("https://api.example.com/app/accountant"), now)
                .unwrap(),
            "b=2"
        );
        assert_eq!(
            jar.header(&uri("https://api.example.com/other/x"), now)
                .unwrap(),
            "c=3; b=2"
        );
        // Secure cookies stay off plain HTTP; host-only ones off other hosts.
        assert_eq!(jar.header(&uri("http://api.example.com/x"), now), None);
        assert_eq!(jar.header(&uri("https://x.api.example.com/"), now), None);
        assert_eq!(jar.pairs(now).len(), 3);
        jar.clear();
        assert!(jar.pairs(now).is_empty());
    }

    #[test]
    fn domains() {
        let now = SystemTime::now();
        let mut jar = CookieJar::default();
        let api = uri("http://api.example.com/");
        jar.store(&api, "d=1; Domain=.Example.com", now);
        jar.store(&api, "e=1; Domain=other.com", now);
        jar.store(&api, "f=1; Domain=", now);
        assert_eq!(
            jar.header(&uri("http://www.example.com/"), now).unwrap(),
            "d=1"
        );
        assert_eq!(jar.header(&uri("http://badexample.com/"), now), None);
        let ip = uri("http://127.0.0.1:8080/x");
        jar.store(&ip, "g=1", now);
        assert_eq!(jar.header(&ip, now).unwrap(), "g=1");
        assert_eq!(jar.header(&uri("http://[::1]/"), now), None);
        assert!(!domain_matches("1.127.0.0.1", "127.0.0.1"));
    }

    #[test]
    fn expiry() {
        let now = SystemTime::now();
        let mut jar = CookieJar::default();
        let api = uri("http://api.example.com/");
        jar.store(&api, "a=1; Max-Age=60", now);
        jar.store(&api, "b=1; Expires=Wed, 21 Oct 2099 07:28:00 GMT", now);
        jar.store(&api, "c=1; Expires=Thu, 01-Jan-1970 00:00:01 GMT", now);
        jar.store(&api, "d=1; Expires=garbage", now);
        jar.store(&api, "e=1; Max-Age=99999999999999", now);
        assert_eq!(jar.header(&api, now).unwrap(), "a=1; b=1; d=1; e=1");
        let later = now + Duration::from_secs(120);
        assert_eq!(jar.header(&api, later).unwrap(), "b=1; d=1; e=1");
        // Max-Age wins over Expires, and zero deletes.
        jar.store(
            &api,
            "b=2; Max-Age=0; Expires=Wed, 21 Oct 2099 07:28:00 GMT",
            now,
        );
        jar.store(&api, "d=2", now);
        assert_eq!(jar.header(&api, now).unwrap(), "d=2; e=1");
    }
}
