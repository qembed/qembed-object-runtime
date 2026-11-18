//! Effective invocation identity and its independent execution budget class.

/// Persisted kind of the effective invocation Caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallerKind {
    /// A server-issued identity for an unauthenticated session.
    Anonymous,
    /// An authenticated end-user or application identity.
    Authenticated,
    /// A service identity acting through an explicit delegation.
    Service,
}

impl CallerKind {
    /// Parses the canonical SQL enum spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Anonymous" => Some(Self::Anonymous),
            "Authenticated" => Some(Self::Authenticated),
            "Service" => Some(Self::Service),
            _ => None,
        }
    }

    /// Returns the canonical SQL enum spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Anonymous => "Anonymous",
            Self::Authenticated => "Authenticated",
            Self::Service => "Service",
        }
    }
}

/// Invocation budget/execution pool stored independently from Caller kind.
///
/// SQL currently supports only these two pool values. There is deliberately no
/// automatic mapping from CallerKind: in particular, Service does not imply a
/// pool. The trusted application policy must choose this value explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InvocationPool {
    Anonymous,
    Authenticated,
}

impl InvocationPool {
    /// Returns the canonical SQL value.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Authenticated => "authenticated",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_kind_parses_only_canonical_sql_values() {
        assert_eq!(CallerKind::parse("Anonymous"), Some(CallerKind::Anonymous));
        assert_eq!(CallerKind::parse("Authenticated"), Some(CallerKind::Authenticated));
        assert_eq!(CallerKind::parse("Service"), Some(CallerKind::Service));
        assert_eq!(CallerKind::parse("service"), None);
        assert_eq!(CallerKind::parse("Owner"), None);
    }

    #[test]
    fn service_caller_does_not_imply_an_invocation_pool() {
        let caller = CallerKind::Service;
        let pool = InvocationPool::Authenticated;
        assert_eq!(caller, CallerKind::Service);
        assert_eq!(pool.as_sql(), "authenticated");
    }
}
