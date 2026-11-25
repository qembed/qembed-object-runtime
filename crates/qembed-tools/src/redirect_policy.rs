//! Redirect policy that revalidates every destination before the next request.
use std::{collections::HashSet, error::Error, fmt};

use url::{Host, Url};

use crate::dns_resolution::{
    DnsName, DnsResolutionError, DnsResolver, ResolvedAddressSet, resolve_public,
};

/// Controls how many redirect responses may be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedirectPolicy {
    max_redirects: usize,
}

impl RedirectPolicy {
    /// Creates a policy with an explicit maximum number of followed redirects.
    #[must_use]
    pub const fn new(max_redirects: usize) -> Self {
        Self { max_redirects }
    }

    /// Validates the initial URL and resolves its complete public A+AAAA address set.
    pub async fn start<R: DnsResolver>(
        &self,
        initial_url: &str,
        resolver: &R,
    ) -> Result<RedirectChain, RedirectError> {
        let url = Url::parse(initial_url)
            .map_err(|error| RedirectError::InvalidUrl(error.to_string()))?;
        let current = validate_target(url, resolver).await?;
        let mut visited = HashSet::new();
        visited.insert(current.url.clone());

        Ok(RedirectChain {
            max_redirects: self.max_redirects,
            redirects_followed: 0,
            visited,
            current,
        })
    }
}

/// A URL and the validated DNS addresses to which the next request must be pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedHttpTarget {
    url: Url,
    addresses: ResolvedAddressSet,
    port: u16,
}

impl ValidatedHttpTarget {
    /// Returns the normalized URL, without a fragment.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the DNS address set bound to this URL's hostname.
    #[must_use]
    pub fn addresses(&self) -> &ResolvedAddressSet {
        &self.addresses
    }

    /// Returns the validated destination port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the hostname bound to this URL and its resolved address set.
    #[must_use]
    pub fn hostname(&self) -> &DnsName {
        self.addresses.hostname()
    }
}

/// State for one redirect chain, including loop detection and the hop limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectChain {
    max_redirects: usize,
    redirects_followed: usize,
    visited: HashSet<Url>,
    current: ValidatedHttpTarget,
}

impl RedirectChain {
    /// Returns the currently validated target. Use its address set for pinned connect.
    #[must_use]
    pub fn current(&self) -> &ValidatedHttpTarget {
        &self.current
    }

    /// Returns the number of successfully followed redirects.
    #[must_use]
    pub const fn redirects_followed(&self) -> usize {
        self.redirects_followed
    }

    /// Resolves and validates a Location value before allowing the next request.
    ///
    /// Call this only after receiving a redirect response. The returned target has a
    /// fresh DNS answer set bound to the redirect destination's own hostname.
    pub async fn follow_location<R: DnsResolver>(
        &mut self,
        location: &str,
        resolver: &R,
    ) -> Result<&ValidatedHttpTarget, RedirectError> {
        if self.redirects_followed >= self.max_redirects {
            return Err(RedirectError::TooManyRedirects {
                limit: self.max_redirects,
            });
        }

        let mut next_url = self
            .current
            .url
            .join(location)
            .map_err(|error| RedirectError::InvalidUrl(error.to_string()))?;
        next_url.set_fragment(None);

        if self.current.url.scheme() == "https" && next_url.scheme() == "http" {
            return Err(RedirectError::HttpsDowngrade);
        }

        if self.visited.contains(&next_url) {
            return Err(RedirectError::RedirectLoop(next_url.to_string()));
        }

        let next = validate_target(next_url, resolver).await?;
        self.visited.insert(next.url.clone());
        self.current = next;
        self.redirects_followed += 1;
        Ok(&self.current)
    }
}

async fn validate_target<R: DnsResolver>(
    mut url: Url,
    resolver: &R,
) -> Result<ValidatedHttpTarget, RedirectError> {
    url.set_fragment(None);

    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(RedirectError::UnsupportedScheme(url.scheme().to_owned()));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(RedirectError::CredentialsNotAllowed);
    }

    let host = match url.host() {
        Some(Host::Domain(host)) => host,
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => {
            return Err(RedirectError::IpLiteralNotAllowed);
        }
        None => return Err(RedirectError::MissingHost),
    };
    let hostname = DnsName::parse(host).map_err(|_| RedirectError::InvalidHostname)?;
    let port = url
        .port_or_known_default()
        .ok_or(RedirectError::InvalidPort)?;
    if port == 0 {
        return Err(RedirectError::InvalidPort);
    }

    let addresses = resolve_public(resolver, hostname.as_fqdn())
        .await
        .map_err(RedirectError::DnsResolution)?;

    Ok(ValidatedHttpTarget {
        url,
        addresses,
        port,
    })
}

/// Errors raised while validating an initial URL or redirect destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedirectError {
    InvalidUrl(String),
    UnsupportedScheme(String),
    CredentialsNotAllowed,
    IpLiteralNotAllowed,
    MissingHost,
    InvalidHostname,
    InvalidPort,
    HttpsDowngrade,
    RedirectLoop(String),
    TooManyRedirects { limit: usize },
    DnsResolution(DnsResolutionError),
}

impl fmt::Display for RedirectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(formatter, "invalid redirect URL: {message}"),
            Self::UnsupportedScheme(scheme) => {
                write!(formatter, "unsupported redirect URL scheme: {scheme}")
            }
            Self::CredentialsNotAllowed => {
                formatter.write_str("credentials in redirect URLs are not allowed")
            }
            Self::IpLiteralNotAllowed => {
                formatter.write_str("IP-literal redirect destinations are not allowed")
            }
            Self::MissingHost => formatter.write_str("redirect URL has no hostname"),
            Self::InvalidHostname => {
                formatter.write_str("redirect URL contains an invalid hostname")
            }
            Self::InvalidPort => formatter.write_str("redirect URL has an invalid port"),
            Self::HttpsDowngrade => {
                formatter.write_str("redirect from HTTPS to HTTP is not allowed")
            }
            Self::RedirectLoop(url) => write!(formatter, "redirect loop detected at {url}"),
            Self::TooManyRedirects { limit } => {
                write!(formatter, "redirect limit of {limit} was exceeded")
            }
            Self::DnsResolution(error) => {
                write!(
                    formatter,
                    "redirect destination DNS validation failed: {error}"
                )
            }
        }
    }
}

impl Error for RedirectError {}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::dns_resolution::{DnsAnswer, DnsRecordFamily};

    #[derive(Clone)]
    struct FixtureResolver {
        answers: HashMap<String, DnsAnswer>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FixtureResolver {
        fn new() -> Self {
            Self {
                answers: HashMap::from([
                    (
                        "public.example.".to_owned(),
                        DnsAnswer::new(vec!["93.184.216.34".to_owned()], Vec::new()),
                    ),
                    (
                        "other.example.".to_owned(),
                        DnsAnswer::new(vec!["1.1.1.1".to_owned()], Vec::new()),
                    ),
                    (
                        "private.example.".to_owned(),
                        DnsAnswer::new(vec!["10.0.0.7".to_owned()], Vec::new()),
                    ),
                    (
                        "third.example.".to_owned(),
                        DnsAnswer::new(vec!["8.8.8.8".to_owned()], Vec::new()),
                    ),
                ]),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("resolver call log").clone()
        }
    }

    impl DnsResolver for FixtureResolver {
        async fn lookup(&self, name: &DnsName) -> Result<DnsAnswer, DnsResolutionError> {
            let hostname = name.as_fqdn().to_owned();
            self.calls
                .lock()
                .expect("resolver call log")
                .push(hostname.clone());

            self.answers
                .get(&hostname)
                .cloned()
                .ok_or_else(|| DnsResolutionError::LookupFailed {
                    family: DnsRecordFamily::A,
                    message: format!("no fixture for {hostname}"),
                })
        }
    }

    #[tokio::test]
    async fn rejects_a_public_to_private_redirect() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);
        let mut chain = policy
            .start("https://public.example/start", &resolver)
            .await
            .expect("public initial target");

        let result = chain
            .follow_location("https://private.example/secret", &resolver)
            .await;

        assert!(matches!(
            result,
            Err(RedirectError::DnsResolution(
                DnsResolutionError::NonPublicAddress(address)
            )) if address == "10.0.0.7".parse::<std::net::IpAddr>().unwrap()
        ));
        assert_eq!(
            chain.current().url().as_str(),
            "https://public.example/start"
        );
        assert_eq!(
            resolver.calls(),
            vec!["public.example.", "private.example."]
        );
    }

    #[tokio::test]
    async fn revalidates_and_allows_a_cross_host_public_redirect() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);
        let mut chain = policy
            .start("https://public.example/start", &resolver)
            .await
            .expect("public initial target");

        let target = chain
            .follow_location("https://other.example/next", &resolver)
            .await
            .expect("public cross-host redirect");

        assert_eq!(target.hostname().as_fqdn(), "other.example.");
        assert_eq!(target.addresses().hostname().as_fqdn(), "other.example.");
        assert_eq!(target.url().as_str(), "https://other.example/next");
        assert_eq!(resolver.calls(), vec!["public.example.", "other.example."]);
    }

    #[tokio::test]
    async fn rejects_redirect_loops_without_resolving_the_repeated_url() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);
        let mut chain = policy
            .start("https://public.example/start", &resolver)
            .await
            .expect("public initial target");
        chain
            .follow_location("https://other.example/next", &resolver)
            .await
            .expect("first redirect");

        let result = chain
            .follow_location("https://public.example/start", &resolver)
            .await;

        assert!(matches!(result, Err(RedirectError::RedirectLoop(_))));
        assert_eq!(resolver.calls(), vec!["public.example.", "other.example."]);
        assert_eq!(chain.redirects_followed(), 1);
    }

    #[tokio::test]
    async fn enforces_the_redirect_limit_before_resolving_another_hop() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(1);
        let mut chain = policy
            .start("https://public.example/start", &resolver)
            .await
            .expect("public initial target");
        chain
            .follow_location("https://other.example/next", &resolver)
            .await
            .expect("first redirect");

        let result = chain
            .follow_location("https://third.example/final", &resolver)
            .await;

        assert_eq!(result, Err(RedirectError::TooManyRedirects { limit: 1 }));
        assert_eq!(resolver.calls(), vec!["public.example.", "other.example."]);
        assert_eq!(chain.redirects_followed(), 1);
    }

    #[tokio::test]
    async fn rejects_ip_literals_credentials_and_unsupported_schemes() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);

        assert_eq!(
            policy.start("https://127.0.0.1/private", &resolver).await,
            Err(RedirectError::IpLiteralNotAllowed)
        );
        assert_eq!(
            policy.start("https://2130706433/private", &resolver).await,
            Err(RedirectError::IpLiteralNotAllowed)
        );
        assert_eq!(
            policy
                .start("https://user:pass@public.example/", &resolver)
                .await,
            Err(RedirectError::CredentialsNotAllowed)
        );
        assert_eq!(
            policy.start("file:///etc/passwd", &resolver).await,
            Err(RedirectError::UnsupportedScheme("file".to_owned()))
        );
        assert!(resolver.calls().is_empty());
    }

    #[tokio::test]
    async fn rejects_https_to_http_downgrades_without_resolving_the_target() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);
        let mut chain = policy
            .start("https://public.example/start", &resolver)
            .await
            .expect("public initial target");

        let result = chain
            .follow_location("http://other.example/insecure", &resolver)
            .await;

        assert_eq!(result, Err(RedirectError::HttpsDowngrade));
        assert_eq!(resolver.calls(), vec!["public.example."]);
    }

    #[tokio::test]
    async fn resolves_relative_locations_against_the_current_url() {
        let resolver = FixtureResolver::new();
        let policy = RedirectPolicy::new(5);
        let mut chain = policy
            .start("https://public.example/a/b", &resolver)
            .await
            .expect("public initial target");

        let target = chain
            .follow_location("../next?x=1#fragment", &resolver)
            .await
            .expect("relative redirect");

        assert_eq!(target.url().as_str(), "https://public.example/next?x=1");
        assert_eq!(target.hostname().as_fqdn(), "public.example.");
        assert_eq!(resolver.calls(), vec!["public.example.", "public.example."]);
    }
}
