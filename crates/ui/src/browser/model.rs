//! Browser state shared by the chrome and platform host. No native handles.
use std::net::IpAddr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageState {
    pub url: Option<String>,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub error: Option<String>,
}

impl PageState {
    pub fn label(&self) -> String {
        if !self.title.trim().is_empty() {
            self.title.clone()
        } else {
            self.url
                .as_deref()
                .and_then(|s| url::Url::parse(s).ok())
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Browser".into())
        }
    }
}

pub fn loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

pub fn normalize_address(input: &str) -> Result<String, &'static str> {
    let text = input.trim();
    if text.is_empty() {
        return Err("Enter a website or localhost address.");
    }
    if text.chars().any(|c| c.is_control()) {
        return Err("This address contains invalid characters.");
    }
    // A bare host:port looks like a URI scheme to a URL parser. Only accept
    // that ambiguity when the suffix is an actual numeric port.
    let authority = text.split(['/', '?', '#']).next().unwrap_or(text);
    let host_port = authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
    let explicit =
        text.contains("://") || (text.contains(':') && !host_port && !text.starts_with('['));
    let mut parsed = url::Url::parse(&if explicit {
        text.to_owned()
    } else {
        format!("https://{text}")
    })
    .map_err(|_| "Enter a valid website or localhost address.")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Only http and https addresses are supported.");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Use an address without an embedded username or password.");
    }
    if !explicit && loopback(&parsed) {
        let _ = parsed.set_scheme("http");
    }
    Ok(parsed.into())
}

pub fn allowed_navigation(address: &str) -> bool {
    url::Url::parse(address).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presentation {
    #[default]
    Hidden,
    Live,
    /// GPUI draws a temporary snapshot while a menu or a pane tween covers
    /// the native child. The live page keeps its history and DOM state.
    Covered,
}

pub fn presentation(active: bool, covered: bool) -> Presentation {
    if !active {
        Presentation::Hidden
    } else if covered {
        Presentation::Covered
    } else {
        Presentation::Live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_web_addresses_and_loopback_ports() {
        for (input, expected) in [
            ("localhost:3000", "http://localhost:3000/"),
            ("127.0.0.1:5173/a?x=1", "http://127.0.0.1:5173/a?x=1"),
            ("[::1]:8080", "http://[::1]:8080/"),
            ("[::1]", "http://[::1]/"),
            (" app.localhost:3000 ", "http://app.localhost:3000/"),
            ("example.com/path", "https://example.com/path"),
            ("https://localhost:3000", "https://localhost:3000/"),
            ("http://example.com", "http://example.com/"),
        ] {
            assert_eq!(normalize_address(input), Ok(expected.into()), "{input}");
        }
    }
    #[test]
    fn rejects_non_web_schemes_credentials_and_bad_input() {
        for input in [
            "",
            "javascript:alert(1)",
            "file:///tmp/a",
            "data:text/html,hi",
            "zeron://open/chat/a",
            "https://user:pass@example.com",
            "https://",
            "two words",
            "https://example.com/\nsecret",
        ] {
            assert!(normalize_address(input).is_err(), "{input}");
        }
        assert!(!allowed_navigation("javascript:alert(1)"));
        assert!(!allowed_navigation("https://user@example.com/"));
    }
    #[test]
    fn native_visibility_never_outlives_its_surface() {
        assert_eq!(presentation(false, false), Presentation::Hidden);
        assert_eq!(presentation(false, true), Presentation::Hidden);
        assert_eq!(presentation(true, true), Presentation::Covered);
        assert_eq!(presentation(true, false), Presentation::Live);
    }
    #[test]
    fn tab_labels_fall_back_to_host_then_browser() {
        let mut page = PageState::default();
        assert_eq!(page.label(), "Browser");
        page.url = Some("http://localhost:3000/path".into());
        assert_eq!(page.label(), "localhost");
        page.title = "Local preview".into();
        assert_eq!(page.label(), "Local preview");
    }
}
