//! Deterministic citation and provenance checks for retrieved Knowledge evidence.
//!
//! This verifier checks provenance and lexical grounding. It is deliberately
//! not a natural-language entailment engine: a valid citation does not prove
//! that prose semantically follows from a chunk.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ids::KnowledgeChunkId;

pub const MAX_CLAIMS: usize = 128;
pub const MAX_RETRIEVED_CHUNKS: usize = 512;
pub const MAX_CLAIM_BYTES: usize = 8 * 1024;
pub const MAX_EVIDENCE_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_CITATIONS_PER_CLAIM: usize = 16;
pub const MAX_CLAIM_ID_BYTES: usize = 64;

/// A structured answer split into independently verifiable claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitationDocument {
    pub claims: Vec<CitationClaim>,
}

/// One factual statement and the retrieved chunks offered as its evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitationClaim {
    pub id: String,
    pub text: String,
    pub citations: Vec<KnowledgeChunkId>,
}

/// A chunk in the exact retrieval result supplied to the verifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetrievedEvidenceChunk {
    pub id: KnowledgeChunkId,
    pub content: String,
}

impl RetrievedEvidenceChunk {
    #[must_use]
    pub fn new(id: KnowledgeChunkId, content: impl Into<String>) -> Self {
        Self { id, content: content.into() }
    }
}

/// Stable verifier rule identifiers. These IDs are part of the audit contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRule {
    /// V1 — bounded, non-empty document and evidence envelope.
    V1Envelope,
    /// V2 — well-formed, unique claim identifiers and bounded claim text.
    V2ClaimShape,
    /// V3 — every claim carries a bounded, non-empty, duplicate-free citation list.
    V3RequiredCitations,
    /// V4 — every citation resolves to exactly one chunk in this retrieval result.
    V4CitationResolution,
    /// V5 — every numeric literal in a claim occurs in at least one cited chunk.
    V5NumericGrounding,
    /// V6 — every explicit URL in a claim occurs in at least one cited chunk.
    V6LinkGrounding,
    /// V7 — malformed or ambiguous evidence input fails closed.
    V7EvidenceIntegrity,
}

/// One deterministic verifier finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationViolation {
    pub rule: VerificationRule,
    pub claim_id: Option<String>,
    pub chunk_id: Option<KnowledgeChunkId>,
    pub detail: String,
}

/// Result of verifying all claims against one retrieved evidence set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerificationReport {
    violations: Vec<VerificationViolation>,
}

impl VerificationReport {
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.violations.is_empty()
    }

    #[must_use]
    pub fn violations(&self) -> &[VerificationViolation] {
        &self.violations
    }
}

/// Stateless deterministic citation/provenance verifier.
#[derive(Clone, Copy, Debug, Default)]
pub struct CitationVerifier;

impl CitationVerifier {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Verifies claim structure, citation resolution, and lexical numeric/link support.
    ///
    /// A claim is rejected unless it has citations resolving to this exact
    /// retrieved-evidence slice. Numbers and explicit URLs must be present in
    /// at least one of those cited chunks. This method does not infer semantic
    /// entailment for ordinary prose.
    #[must_use]
    pub fn verify(
        &self,
        document: &CitationDocument,
        retrieved: &[RetrievedEvidenceChunk],
    ) -> VerificationReport {
        let mut report = VerificationReport::default();

        if document.claims.is_empty() || document.claims.len() > MAX_CLAIMS {
            report.violations.push(violation(
                VerificationRule::V1Envelope, None, None,
                "claim count must be between 1 and MAX_CLAIMS",
            ));
        }
        if retrieved.is_empty() || retrieved.len() > MAX_RETRIEVED_CHUNKS {
            report.violations.push(violation(
                VerificationRule::V1Envelope, None, None,
                "retrieved evidence count must be between 1 and MAX_RETRIEVED_CHUNKS",
            ));
        }

        let mut evidence = HashMap::with_capacity(retrieved.len().min(MAX_RETRIEVED_CHUNKS));
        let mut invalid_evidence = false;
        for chunk in retrieved.iter().take(MAX_RETRIEVED_CHUNKS) {
            if chunk.content.trim().is_empty() || chunk.content.len() > MAX_EVIDENCE_CHUNK_BYTES {
                invalid_evidence = true;
                report.violations.push(violation(
                    VerificationRule::V7EvidenceIntegrity, None, Some(chunk.id),
                    "retrieved chunk content is empty or exceeds the byte limit",
                ));
            }
            if evidence.insert(chunk.id, chunk.content.as_str()).is_some() {
                invalid_evidence = true;
                report.violations.push(violation(
                    VerificationRule::V7EvidenceIntegrity, None, Some(chunk.id),
                    "retrieved evidence contains a duplicate chunk ID",
                ));
            }
        }

        if invalid_evidence {
            return report;
        }

        let mut claim_ids = HashSet::with_capacity(document.claims.len().min(MAX_CLAIMS));
        for claim in document.claims.iter().take(MAX_CLAIMS) {
            let claim_id_valid = valid_claim_id(&claim.id);
            if !claim_id_valid || !claim_ids.insert(claim.id.as_str()) {
                report.violations.push(violation(
                    VerificationRule::V2ClaimShape, Some(&claim.id), None,
                    "claim ID is malformed or duplicated",
                ));
            }
            if claim.text.trim().is_empty()
                || claim.text.len() > MAX_CLAIM_BYTES
                || claim.text.chars().any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
            {
                report.violations.push(violation(
                    VerificationRule::V2ClaimShape, Some(&claim.id), None,
                    "claim text is empty, contains forbidden control characters, or exceeds the byte limit",
                ));
                continue;
            }

            if claim.citations.is_empty() || claim.citations.len() > MAX_CITATIONS_PER_CLAIM {
                report.violations.push(violation(
                    VerificationRule::V3RequiredCitations, Some(&claim.id), None,
                    "each claim must contain between 1 and MAX_CITATIONS_PER_CLAIM citations",
                ));
                continue;
            }

            let mut cited_ids = HashSet::with_capacity(claim.citations.len());
            let mut cited_content = Vec::with_capacity(claim.citations.len());
            let mut citation_list_valid = true;
            for citation in &claim.citations {
                if !cited_ids.insert(*citation) {
                    citation_list_valid = false;
                    report.violations.push(violation(
                        VerificationRule::V3RequiredCitations, Some(&claim.id), Some(*citation),
                        "claim contains a duplicate citation",
                    ));
                    continue;
                }
                match evidence.get(citation) {
                    Some(content) => cited_content.push(*content),
                    None => {
                        citation_list_valid = false;
                        report.violations.push(violation(
                            VerificationRule::V4CitationResolution, Some(&claim.id), Some(*citation),
                            "citation does not resolve to a chunk in the supplied retrieval result",
                        ));
                    }
                }
            }
            if !citation_list_valid {
                continue;
            }

            let cited_text = cited_content.join("\n");
            let supported_numbers = extract_numbers(&cited_text);
            for number in extract_numbers(&claim.text) {
                if !supported_numbers.contains(&number) {
                    report.violations.push(violation(
                        VerificationRule::V5NumericGrounding, Some(&claim.id), None,
                        "numeric literal is absent from all cited chunks",
                    ));
                }
            }

            let supported_urls = extract_urls(&cited_text);
            for url in extract_urls(&claim.text) {
                if !supported_urls.contains(&url) {
                    report.violations.push(violation(
                        VerificationRule::V6LinkGrounding, Some(&claim.id), None,
                        "explicit URL is absent from all cited chunks",
                    ));
                }
            }
        }

        report
    }
}

fn violation(
    rule: VerificationRule,
    claim_id: Option<&str>,
    chunk_id: Option<KnowledgeChunkId>,
    detail: &str,
) -> VerificationViolation {
    VerificationViolation {
        rule,
        claim_id: claim_id.map(str::to_owned),
        chunk_id,
        detail: detail.to_owned(),
    }
}

fn valid_claim_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CLAIM_ID_BYTES
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn extract_numbers(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut numbers = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() {
            if bytes[index].is_ascii_digit() {
                index += 1;
            } else if (bytes[index] == b'.' || bytes[index] == b',')
                && index + 1 < bytes.len()
                && bytes[index + 1].is_ascii_digit()
            {
                index += 1;
            } else {
                break;
            }
        }
        let mut end = index;
        if end < bytes.len() && bytes[end] == b'%' {
            end += 1;
            index += 1;
        }
        numbers.insert(normalize_number(&text[start..end]));
    }
    numbers
}

fn normalize_number(raw: &str) -> String {
    let percent = raw.ends_with('%');
    let value = raw.trim_end_matches('%');
    let last_dot = value.rfind('.');
    let last_comma = value.rfind(',');
    let decimal_separator = match (last_dot, last_comma) {
        (Some(dot), Some(comma)) => Some(if dot > comma { '.' } else { ',' }),
        (Some(_), None) => Some('.'),
        (None, Some(comma)) => {
            let digits_after = value.len().saturating_sub(comma + 1);
            if digits_after == 3 && value[..comma].len() <= 3 { None } else { Some(',') }
        }
        (None, None) => None,
    };

    let mut normalized = String::with_capacity(value.len() + usize::from(percent));
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            normalized.push(ch);
        } else if Some(ch) == decimal_separator {
            normalized.push('.');
        }
    }
    if percent {
        normalized.push('%');
    }
    normalized
}

fn extract_urls(text: &str) -> BTreeSet<String> {
    let lower = text.to_ascii_lowercase();
    let mut urls = BTreeSet::new();
    for prefix in ["https://", "http://", "www."] {
        let mut offset = 0;
        while let Some(relative) = lower[offset..].find(prefix) {
            let start = offset + relative;
            let preceding_is_word = start > 0 && lower.as_bytes()[start - 1].is_ascii_alphanumeric();
            if preceding_is_word {
                offset = start + prefix.len();
                continue;
            }
            let mut end = start;
            while end < text.len() {
                let byte = text.as_bytes()[end];
                if byte.is_ascii_whitespace() || matches!(byte, b'<' | b'>' | b'"' | b'\'' | b')' | b']' | b'}') {
                    break;
                }
                end += 1;
            }
            let candidate = text[start..end]
                .trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?'));
            if !candidate.is_empty() {
                urls.insert(candidate.to_owned());
            }
            offset = end.max(start + prefix.len());
            if offset >= lower.len() {
                break;
            }
        }
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(content: &str) -> RetrievedEvidenceChunk {
        RetrievedEvidenceChunk::new(KnowledgeChunkId::new(), content)
    }

    fn claim(text: &str, citations: Vec<KnowledgeChunkId>) -> CitationClaim {
        CitationClaim { id: "claim-1".to_owned(), text: text.to_owned(), citations }
    }

    #[test]
    fn accepts_claim_with_resolved_citation_and_grounded_number_and_link() {
        let evidence = chunk("The service supports 128 requests. See https://example.com/docs.");
        let id = evidence.id;
        let document = CitationDocument {
            claims: vec![claim("The service supports 128 requests; see https://example.com/docs.", vec![id])],
        };
        let report = CitationVerifier::new().verify(&document, &[evidence]);
        assert!(report.is_accepted(), "{:?}", report.violations());
    }

    #[test]
    fn rejects_missing_required_citations() {
        let evidence = chunk("A fact.");
        let document = CitationDocument { claims: vec![claim("A fact.", vec![])] };
        let report = CitationVerifier::new().verify(&document, &[evidence]);
        assert!(report.violations().iter().any(|item| item.rule == VerificationRule::V3RequiredCitations));
    }

    #[test]
    fn rejects_citation_not_in_retrieval_result() {
        let evidence = chunk("A fact.");
        let document = CitationDocument {
            claims: vec![claim("A fact.", vec![KnowledgeChunkId::new()])],
        };
        let report = CitationVerifier::new().verify(&document, &[evidence]);
        assert!(report.violations().iter().any(|item| item.rule == VerificationRule::V4CitationResolution));
    }

    #[test]
    fn rejects_unsupported_numbers_and_links_independently() {
        let evidence = chunk("The limit is 128. See https://example.com/source.");
        let id = evidence.id;
        let document = CitationDocument {
            claims: vec![claim("The limit is 256. See https://example.com/other.", vec![id])],
        };
        let report = CitationVerifier::new().verify(&document, &[evidence]);
        assert!(report.violations().iter().any(|item| item.rule == VerificationRule::V5NumericGrounding));
        assert!(report.violations().iter().any(|item| item.rule == VerificationRule::V6LinkGrounding));
    }

    #[test]
    fn rejects_duplicate_claim_and_evidence_ids() {
        let evidence = chunk("A fact.");
        let id = evidence.id;
        let document = CitationDocument {
            claims: vec![
                claim("First fact.", vec![id]),
                CitationClaim { id: "claim-1".to_owned(), text: "Second fact.".to_owned(), citations: vec![id] },
            ],
        };
        let duplicate_claim = CitationVerifier::new().verify(&document, &[evidence.clone()]);
        assert!(duplicate_claim.violations().iter().any(|item| item.rule == VerificationRule::V2ClaimShape));

        let duplicate_evidence = CitationVerifier::new().verify(&document, &[evidence.clone(), evidence]);
        assert!(duplicate_evidence.violations().iter().any(|item| item.rule == VerificationRule::V7EvidenceIntegrity));
    }

    #[test]
    fn rejects_malformed_and_unbounded_claim_envelopes() {
        let evidence = chunk("A fact.");
        let id = evidence.id;
        let malformed = CitationDocument {
            claims: vec![
                CitationClaim { id: "bad id".to_owned(), text: "Fact".to_owned(), citations: vec![id] },
                CitationClaim { id: "claim-2".to_owned(), text: " \n ".to_owned(), citations: vec![id] },
                CitationClaim { id: "claim-3".to_owned(), text: "x".repeat(MAX_CLAIM_BYTES + 1), citations: vec![id] },
                CitationClaim { id: "claim-4".to_owned(), text: "bad\u{0000}text".to_owned(), citations: vec![id] },
            ],
        };
        let report = CitationVerifier::new().verify(&malformed, &[evidence]);
        assert!(report.violations().iter().any(|item| item.rule == VerificationRule::V2ClaimShape));
    }

    #[test]
    fn generated_malformed_claims_fail_closed() {
        let evidence = chunk("A source without numeric or URL requirements.");
        let id = evidence.id;
        let verifier = CitationVerifier::new();
        let long_id = "x".repeat(MAX_CLAIM_ID_BYTES + 1);
        let invalid_ids = ["", " ", "has space", "slash/name", long_id.as_str()];

        for invalid_id in invalid_ids {
            let document = CitationDocument {
                claims: vec![claim_with_id(invalid_id, "A claim.", id)],
            };
            assert!(!verifier.verify(&document, std::slice::from_ref(&evidence)).is_accepted());
        }

        for length in 0..=MAX_CLAIM_BYTES + 2 {
            let text = "x".repeat(length);
            let document = CitationDocument { claims: vec![claim(&text, vec![id])] };
            let accepted = verifier.verify(&document, std::slice::from_ref(&evidence)).is_accepted();
            assert_eq!(accepted, length > 0 && length <= MAX_CLAIM_BYTES);
        }

        for codepoint in 0u32..=0x1f {
            let ch = char::from_u32(codepoint).expect("ASCII control codepoint");
            if ch == '\n' || ch == '\t' {
                continue;
            }
            let document = CitationDocument { claims: vec![claim(&format!("bad{ch}text"), vec![id])] };
            assert!(!verifier.verify(&document, std::slice::from_ref(&evidence)).is_accepted());
        }
    }

    fn claim_with_id(id: &str, text: &str, citation: KnowledgeChunkId) -> CitationClaim {
        CitationClaim { id: id.to_owned(), text: text.to_owned(), citations: vec![citation] }
    }

    #[test]
    fn normalizes_grouped_and_decimal_numbers_without_losing_percent_units() {
        let evidence = chunk("Values: 1,000; 3.14; 12,5; and 5%.");
        let id = evidence.id;
        let document = CitationDocument {
            claims: vec![claim("Values: 1000, 3.14, 12.5, and 5%.", vec![id])],
        };
        assert!(CitationVerifier::new().verify(&document, &[evidence]).is_accepted());
    }

    #[test]
    fn rejects_malformed_json_shapes_and_unknown_fields() {
        assert!(serde_json::from_str::<CitationDocument>(r#"{"claims":[],"unexpected":true}"#).is_err());
        assert!(serde_json::from_str::<CitationDocument>(r#"{"claims":[{"id":"x","text":"ok","citations":[],"extra":1}]}"#).is_err());
        assert!(serde_json::from_str::<CitationDocument>(r#"{"claims":[{"id":"x","text":"ok","citations":["not-a-uuid"]}]}"#).is_err());
    }
}
