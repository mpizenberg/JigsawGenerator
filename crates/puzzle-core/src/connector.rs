use kurbo::CubicBez;
use rand_chacha::ChaCha8Rng;

use crate::edge::EdgeParams;

/// Trait for generating connector shapes on internal puzzle edges.
///
/// Implementations produce bezier curves in edge-local coordinates
/// where (0, 0) is the edge start and (length, 0) is the edge end.
/// The grid engine controls the RNG sequence for determinism by
/// passing the RNG as a parameter.
///
/// Border edges (straight lines + optional rounded corners) are
/// handled separately outside this trait.
pub trait ConnectorGenerator: Send + Sync {
    /// Generate bezier curves for an edge connector.
    ///
    /// Returns control points in edge-local coordinates
    /// (0,0 = edge start, (length, 0) = edge end).
    fn generate(&self, params: &EdgeParams, rng: &mut ChaCha8Rng) -> Vec<CubicBez>;

    /// Validate that generated curves stay within acceptable bounds.
    ///
    /// Default: check bounding box doesn't exceed 5% beyond nominal
    /// piece boundary.
    fn validate(&self, curves: &[CubicBez], params: &EdgeParams) -> Result<(), String>;
}

/// Minimal connector implementation for testing.
///
/// Returns no curves and always validates successfully.
/// Allows the grid engine to be tested without real connector shapes.
pub struct NullConnector;

impl ConnectorGenerator for NullConnector {
    fn generate(&self, _params: &EdgeParams, _rng: &mut ChaCha8Rng) -> Vec<CubicBez> {
        Vec::new()
    }

    fn validate(&self, _curves: &[CubicBez], _params: &EdgeParams) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::TabDirection;
    use crate::seed::create_rng;

    #[test]
    fn test_null_connector_generate_empty() {
        let connector = NullConnector;
        let params = EdgeParams {
            length: 50.0,
            cross_length: 50.0,
            direction: TabDirection::Out,
            offset: 0.0,
            knob_scale: 1.0,
        };
        let mut rng = create_rng("test");
        let curves = connector.generate(&params, &mut rng);
        assert!(curves.is_empty());
    }

    #[test]
    fn test_null_connector_validate_ok() {
        let connector = NullConnector;
        let params = EdgeParams {
            length: 50.0,
            cross_length: 50.0,
            direction: TabDirection::In,
            offset: 0.0,
            knob_scale: 1.0,
        };
        let result = connector.validate(&[], &params);
        assert!(result.is_ok());
    }

    #[test]
    fn test_null_connector_implements_trait() {
        // Verify NullConnector can be used as a trait object
        let connector: Box<dyn ConnectorGenerator> = Box::new(NullConnector);
        let params = EdgeParams {
            length: 30.0,
            cross_length: 30.0,
            direction: TabDirection::Out,
            offset: 0.0,
            knob_scale: 1.0,
        };
        let mut rng = create_rng("trait-test");
        let curves = connector.generate(&params, &mut rng);
        assert!(curves.is_empty());
        assert!(connector.validate(&curves, &params).is_ok());
    }
}
