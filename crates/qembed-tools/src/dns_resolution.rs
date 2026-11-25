//! Full A+AAAA DNS resolution and fail-closed address-set validation.
use std::{
    error::Error,
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use hickory_resolver::{Resolver, TokioResolver, proto::rr::RData};

/// A canonical absolute DNS hostname. IP literals and relative search names are rejected.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DnsName(String);

impl DnsName {
    /// Parses a DNS hostname and canonicalizes it to an absolute lowercase FQDN.
    pub fn parse(input: &str) -> Result<Self, DnsResolutionError> {
        if input.is_empty() || input.trim() != input || input.parse::<IpAddr>().is_ok() {
            return Err(DnsResolutionError::InvalidHostname);
        }

        let without_root = input.strip_suffix('.').unwrap_or(input);
        if without_root.is_empty() || without_root.len() > 253 {
            return Err(DnsResolutionError::InvalidHostname);
        }

        let labels: Vec<&str> = without_root.split('.').collect();
        if labels.len() < 2
            || labels.iter().any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || !label.as_bytes()[0].is_ascii_alphanumeric()
                    || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            || labels
                .iter()
                .all(|label| label.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(DnsResolutionError::InvalidHostname);
        }

        let normalized = without_root.to_ascii_lowercase();
        if normalized == "localhost" || normalized.ends_with(".localhost") {
            return Err(DnsResolutionError::InvalidHostname);
        }

        Ok(Self(format!("{normalized}.")))
    }

    /// Returns the absolute DNS name, including its final dot.
    #[must_use]
    pub fn as_fqdn(&self) -> &str {
        &self.0
    }
}

/// DNS address records returned by a resolver before parsing and policy validation.
/// Raw strings make malformed fixtures testable; callers must use resolve_public.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DnsAnswer {
    a_records: Vec<String>,
    aaaa_records: Vec<String>,
}

impl DnsAnswer {
    /// Creates an untrusted A/AAAA answer set.
    #[must_use]
    pub fn new(a_records: Vec<String>, aaaa_records: Vec<String>) -> Self {
        Self {
            a_records,
            aaaa_records,
        }
    }
}

/// Address family used in resolver and parsing errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsRecordFamily {
    A,
    Aaaa,
}

impl fmt::Display for DnsRecordFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::A => formatter.write_str("A"),
            Self::Aaaa => formatter.write_str("AAAA"),
        }
    }
}

/// A fully parsed, deduplicated and validated DNS answer set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddressSet {
    addresses: Vec<IpAddr>,
}

impl ResolvedAddressSet {
    /// Returns every validated address from both record families.
    #[must_use]
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

/// Errors produced while resolving or validating an address set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsResolutionError {
    InvalidHostname,
    ResolverInitialization(String),
    LookupFailed {
        family: DnsRecordFamily,
        message: String,
    },
    EmptyAnswer,
    MalformedAddress {
        family: DnsRecordFamily,
        value: String,
    },
    NonPublicAddress(IpAddr),
}

impl fmt::Display for DnsResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostname => {
                formatter.write_str("hostname is not a valid absolute DNS name")
            }
            Self::ResolverInitialization(message) => {
                write!(formatter, "DNS resolver initialization failed: {message}")
            }
            Self::LookupFailed { family, message } => {
                write!(formatter, "{family} DNS lookup failed: {message}")
            }
            Self::EmptyAnswer => formatter.write_str("DNS answer contains no A or AAAA records"),
            Self::MalformedAddress { family, value } => {
                write!(formatter, "malformed {family} address record: {value}")
            }
            Self::NonPublicAddress(address) => {
                write!(
                    formatter,
                    "DNS answer contains a non-public address: {address}"
                )
            }
        }
    }
}

impl Error for DnsResolutionError {}

/// Injectable boundary for retrieving both A and AAAA records.
pub trait DnsResolver: Send + Sync {
    /// Returns the complete A and AAAA answer sets for an absolute hostname.
    fn lookup(
        &self,
        name: &DnsName,
    ) -> impl Future<Output = Result<DnsAnswer, DnsResolutionError>> + Send;
}

/// Hickory-based resolver configured from the host's recursive DNS configuration.
#[derive(Clone)]
pub struct HickoryDnsResolver {
    resolver: TokioResolver,
}

impl HickoryDnsResolver {
    /// Builds a Tokio resolver from system DNS configuration.
    pub fn new() -> Result<Self, DnsResolutionError> {
        let resolver = Resolver::builder_tokio()
            .map_err(|error| DnsResolutionError::ResolverInitialization(error.to_string()))?
            .build()
            .map_err(|error| DnsResolutionError::ResolverInitialization(error.to_string()))?;
        Ok(Self { resolver })
    }
}

impl DnsResolver for HickoryDnsResolver {
    async fn lookup(&self, name: &DnsName) -> Result<DnsAnswer, DnsResolutionError> {
        // Query the two families independently. lookup_ip() may hide a failed family
        // when the other family succeeds, which is unsafe for complete-RRset validation.
        let (a_result, aaaa_result) = tokio::join!(
            self.resolver.ipv4_lookup(name.as_fqdn()),
            self.resolver.ipv6_lookup(name.as_fqdn())
        );

        let a_records = match a_result {
            Ok(lookup) => lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::A(address) => Some(address.0.to_string()),
                    _ => None,
                })
                .collect(),
            Err(error) if error.is_no_records_found() && !error.is_nx_domain() => Vec::new(),
            Err(error) => {
                return Err(DnsResolutionError::LookupFailed {
                    family: DnsRecordFamily::A,
                    message: error.to_string(),
                });
            }
        };

        let aaaa_records = match aaaa_result {
            Ok(lookup) => lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::AAAA(address) => Some(address.0.to_string()),
                    _ => None,
                })
                .collect(),
            Err(error) if error.is_no_records_found() && !error.is_nx_domain() => Vec::new(),
            Err(error) => {
                return Err(DnsResolutionError::LookupFailed {
                    family: DnsRecordFamily::Aaaa,
                    message: error.to_string(),
                });
            }
        };

        Ok(DnsAnswer::new(a_records, aaaa_records))
    }
}

/// Resolves a hostname and validates every address in the complete A+AAAA answer.
pub async fn resolve_public<R: DnsResolver>(
    resolver: &R,
    hostname: &str,
) -> Result<ResolvedAddressSet, DnsResolutionError> {
    let name = DnsName::parse(hostname)?;
    let answer = resolver.lookup(&name).await?;
    validate_answer(answer)
}

/// Parses and validates all records. One forbidden or malformed address rejects the
/// entire answer set; no safe subset is returned.
pub fn validate_answer(answer: DnsAnswer) -> Result<ResolvedAddressSet, DnsResolutionError> {
    let mut addresses = Vec::with_capacity(answer.a_records.len() + answer.aaaa_records.len());

    for value in answer.a_records {
        let address =
            value
                .parse::<Ipv4Addr>()
                .map_err(|_| DnsResolutionError::MalformedAddress {
                    family: DnsRecordFamily::A,
                    value: value.clone(),
                })?;
        addresses.push(IpAddr::V4(address));
    }

    for value in answer.aaaa_records {
        let address =
            value
                .parse::<Ipv6Addr>()
                .map_err(|_| DnsResolutionError::MalformedAddress {
                    family: DnsRecordFamily::Aaaa,
                    value: value.clone(),
                })?;
        addresses.push(IpAddr::V6(address));
    }

    if addresses.is_empty() {
        return Err(DnsResolutionError::EmptyAnswer);
    }

    for address in &addresses {
        if !is_public_unicast(*address) {
            return Err(DnsResolutionError::NonPublicAddress(*address));
        }
    }

    addresses.sort_unstable();
    addresses.dedup();
    Ok(ResolvedAddressSet { addresses })
}

fn is_public_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_private()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_broadcast()
        }
        IpAddr::V6(address) => {
            address.to_ipv4_mapped().is_none()
                && !address.is_loopback()
                && !address.is_unique_local()
                && !address.is_unicast_link_local()
                && !address.is_unspecified()
                && !address.is_multicast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FixtureResolver {
        answer: Result<DnsAnswer, DnsResolutionError>,
    }

    impl DnsResolver for FixtureResolver {
        async fn lookup(&self, _name: &DnsName) -> Result<DnsAnswer, DnsResolutionError> {
            self.answer.clone()
        }
    }

    fn resolver(a: &[&str], aaaa: &[&str]) -> FixtureResolver {
        FixtureResolver {
            answer: Ok(DnsAnswer::new(
                a.iter().map(|value| (*value).to_owned()).collect(),
                aaaa.iter().map(|value| (*value).to_owned()).collect(),
            )),
        }
    }

    #[tokio::test]
    async fn preserves_every_public_a_and_aaaa_record() {
        let resolver = resolver(
            &["93.184.216.34", "1.1.1.1"],
            &["2606:4700:4700::1111", "2001:4860:4860::8888"],
        );

        let result = resolve_public(&resolver, "Example.COM")
            .await
            .expect("public RRset");

        assert_eq!(
            result.addresses(),
            &[
                "1.1.1.1".parse::<IpAddr>().unwrap(),
                "93.184.216.34".parse::<IpAddr>().unwrap(),
                "2001:4860:4860::8888".parse::<IpAddr>().unwrap(),
                "2606:4700:4700::1111".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn accepts_one_empty_family_when_the_other_has_records() {
        let resolver = resolver(&["93.184.216.34"], &[]);
        let result = resolve_public(&resolver, "example.com")
            .await
            .expect("A-only answer is valid");
        assert_eq!(
            result.addresses(),
            &["93.184.216.34".parse::<IpAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn rejects_the_whole_mixed_public_and_private_rrset() {
        let resolver = resolver(&["93.184.216.34", "10.0.0.7"], &[]);
        assert_eq!(
            resolve_public(&resolver, "example.com").await,
            Err(DnsResolutionError::NonPublicAddress(
                "10.0.0.7".parse().unwrap()
            ))
        );
    }

    #[tokio::test]
    async fn rejects_an_empty_answer() {
        let resolver = resolver(&[], &[]);
        assert_eq!(
            resolve_public(&resolver, "example.com").await,
            Err(DnsResolutionError::EmptyAnswer)
        );
    }

    #[tokio::test]
    async fn rejects_malformed_address_records() {
        let resolver = resolver(&["93.184.216.34", "not-an-ip"], &[]);
        assert!(matches!(
            resolve_public(&resolver, "example.com").await,
            Err(DnsResolutionError::MalformedAddress {
                family: DnsRecordFamily::A,
                ..
            })
        ));
    }

    #[test]
    fn rejects_special_addresses_from_both_families() {
        for answer in [
            DnsAnswer::new(vec!["127.0.0.1".into()], vec![]),
            DnsAnswer::new(vec![], vec!["::1".into()]),
            DnsAnswer::new(vec!["169.254.1.1".into()], vec![]),
            DnsAnswer::new(vec![], vec!["fc00::1".into()]),
        ] {
            assert!(matches!(
                validate_answer(answer),
                Err(DnsResolutionError::NonPublicAddress(_))
            ));
        }
    }

    #[test]
    fn rejects_invalid_and_ambiguous_hostnames_before_lookup() {
        for hostname in [
            "",
            "localhost",
            "service.localhost",
            "127.0.0.1",
            "2130706433",
            "127.1",
            "example..com",
            "-bad.example",
            "bad-.example",
            "bad name.example",
        ] {
            assert_eq!(
                DnsName::parse(hostname),
                Err(DnsResolutionError::InvalidHostname),
                "{hostname}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_nxdomain_or_transport_errors_instead_of_using_partial_family() {
        let resolver = FixtureResolver {
            answer: Err(DnsResolutionError::LookupFailed {
                family: DnsRecordFamily::Aaaa,
                message: "SERVFAIL".into(),
            }),
        };
        assert!(matches!(
            resolve_public(&resolver, "example.com").await,
            Err(DnsResolutionError::LookupFailed {
                family: DnsRecordFamily::Aaaa,
                ..
            })
        ));
    }
}
