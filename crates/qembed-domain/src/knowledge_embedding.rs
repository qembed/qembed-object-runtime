//! Provider-neutral embedding model contracts and vector compatibility checks.

use std::fmt;

/// Similarity/distance function used to compare vectors produced by a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmbeddingDistanceMetric {
    /// Cosine distance (lower is closer).
    Cosine,
    /// Euclidean distance (lower is closer).
    L2,
    /// Inner product; callers must preserve the backend's score semantics.
    InnerProduct,
}

impl EmbeddingDistanceMetric {
    #[must_use]
    pub const fn as_database_value(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::L2 => "l2",
            Self::InnerProduct => "ip",
        }
    }

    pub fn from_database_value(value: &str) -> Result<Self, EmbeddingModelError> {
        match value {
            "cosine" => Ok(Self::Cosine),
            "l2" => Ok(Self::L2),
            "ip" => Ok(Self::InnerProduct),
            _ => Err(EmbeddingModelError::UnsupportedDistanceMetric),
        }
    }
}

/// Immutable identity and vector-space contract for one registered model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingModelSpec {
    id: String,
    provider: String,
    model_name: String,
    dimension: usize,
    distance_metric: EmbeddingDistanceMetric,
}

impl EmbeddingModelSpec {
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        model_name: impl Into<String>,
        dimension: usize,
        distance_metric: EmbeddingDistanceMetric,
    ) -> Result<Self, EmbeddingModelError> {
        let id = id.into();
        let provider = provider.into();
        let model_name = model_name.into();

        if id.trim().is_empty() || provider.trim().is_empty() || model_name.trim().is_empty() {
            return Err(EmbeddingModelError::MissingIdentity);
        }
        if dimension == 0 || dimension > i32::MAX as usize {
            return Err(EmbeddingModelError::InvalidDimension);
        }

        Ok(Self {
            id,
            provider,
            model_name,
            dimension,
            distance_metric,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    #[must_use]
    pub const fn distance_metric(&self) -> EmbeddingDistanceMetric {
        self.distance_metric
    }
}

/// A registered model. Its specification is immutable; only activation is mutable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingModel {
    spec: EmbeddingModelSpec,
    active: bool,
}

impl EmbeddingModel {
    #[must_use]
    pub const fn new(spec: EmbeddingModelSpec, active: bool) -> Self {
        Self { spec, active }
    }

    #[must_use]
    pub const fn spec(&self) -> &EmbeddingModelSpec {
        &self.spec
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Validates that a vector belongs to this exact model and vector space.
    pub fn validate_vector(&self, vector: &EmbeddingVector) -> Result<(), EmbeddingModelError> {
        if vector.model_id != self.spec.id {
            return Err(EmbeddingModelError::ModelIdMismatch);
        }
        if vector.values.len() != self.spec.dimension {
            return Err(EmbeddingModelError::DimensionMismatch {
                expected: self.spec.dimension,
                actual: vector.values.len(),
            });
        }
        if vector.values.iter().any(|value| !value.is_finite()) {
            return Err(EmbeddingModelError::NonFiniteValue);
        }
        Ok(())
    }
}

/// A vector tagged with the model identity that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingVector {
    model_id: String,
    values: Vec<f32>,
}

impl EmbeddingVector {
    pub fn new(
        model_id: impl Into<String>,
        values: Vec<f32>,
    ) -> Result<Self, EmbeddingModelError> {
        let model_id = model_id.into();

        if model_id.trim().is_empty() || values.is_empty() {
            return Err(EmbeddingModelError::InvalidVector);
        }
        if values.iter().any(|value| !value.is_finite()) {
            return Err(EmbeddingModelError::NonFiniteValue);
        }

        Ok(Self { model_id, values })
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }
}

/// Invalid model metadata or a vector incompatible with its registered model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingModelError {
    MissingIdentity,
    InvalidDimension,
    UnsupportedDistanceMetric,
    InvalidVector,
    ModelIdMismatch,
    DimensionMismatch { expected: usize, actual: usize },
    NonFiniteValue,
}

impl fmt::Display for EmbeddingModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for EmbeddingModelError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, dimension: usize, metric: EmbeddingDistanceMetric) -> EmbeddingModelSpec {
        EmbeddingModelSpec::new(id, "test-provider", "test-model", dimension, metric).unwrap()
    }

    #[test]
    fn dimensions_are_positive_and_bounded_by_postgres_integer() {
        assert_eq!(
            EmbeddingModelSpec::new("m", "p", "n", 0, EmbeddingDistanceMetric::Cosine),
            Err(EmbeddingModelError::InvalidDimension)
        );
        assert_eq!(
            EmbeddingModelSpec::new(
                "m",
                "p",
                "n",
                i32::MAX as usize + 1,
                EmbeddingDistanceMetric::Cosine
            ),
            Err(EmbeddingModelError::InvalidDimension)
        );
    }

    #[test]
    fn all_database_metrics_round_trip_without_aliasing() {
        for metric in [
            EmbeddingDistanceMetric::Cosine,
            EmbeddingDistanceMetric::L2,
            EmbeddingDistanceMetric::InnerProduct,
        ] {
            assert_eq!(
                EmbeddingDistanceMetric::from_database_value(metric.as_database_value()),
                Ok(metric)
            );
        }
        assert_eq!(
            EmbeddingDistanceMetric::from_database_value("euclidean"),
            Err(EmbeddingModelError::UnsupportedDistanceMetric)
        );
    }

    #[test]
    fn vectors_must_match_both_model_identity_and_dimension() {
        let first = EmbeddingModel::new(spec("model-a", 3, EmbeddingDistanceMetric::Cosine), true);
        let second = EmbeddingModel::new(spec("model-b", 3, EmbeddingDistanceMetric::Cosine), true);
        let wrong_model = EmbeddingVector::new("model-b", vec![0.1, 0.2, 0.3]).unwrap();

        assert_eq!(
            first.validate_vector(&wrong_model),
            Err(EmbeddingModelError::ModelIdMismatch)
        );

        let wrong_dimension = EmbeddingVector::new("model-a", vec![0.1, 0.2]).unwrap();
        assert_eq!(
            first.validate_vector(&wrong_dimension),
            Err(EmbeddingModelError::DimensionMismatch {
                expected: 3,
                actual: 2
            })
        );

        let valid = EmbeddingVector::new("model-a", vec![0.1, 0.2, 0.3]).unwrap();
        assert_eq!(first.validate_vector(&valid), Ok(()));
        assert_eq!(
            second.validate_vector(&valid),
            Err(EmbeddingModelError::ModelIdMismatch)
        );
    }

    #[test]
    fn same_dimension_does_not_make_different_metrics_interchangeable() {
        let cosine = EmbeddingModel::new(spec("cosine-model", 3, EmbeddingDistanceMetric::Cosine), true);
        let ip = EmbeddingModel::new(spec("ip-model", 3, EmbeddingDistanceMetric::InnerProduct), true);
        let vector = EmbeddingVector::new("cosine-model", vec![1.0, 0.0, 0.0]).unwrap();

        assert_eq!(cosine.validate_vector(&vector), Ok(()));
        assert_eq!(
            ip.validate_vector(&vector),
            Err(EmbeddingModelError::ModelIdMismatch)
        );
        assert_ne!(
            cosine.spec().distance_metric(),
            ip.spec().distance_metric()
        );
    }

    #[test]
    fn vectors_reject_non_finite_values() {
        assert_eq!(
            EmbeddingVector::new("model-a", vec![f32::NAN]),
            Err(EmbeddingModelError::NonFiniteValue)
        );
    }
}
