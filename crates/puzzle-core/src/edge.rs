use kurbo::{CubicBez, Point};
use serde::{Deserialize, Serialize};

/// Direction a tab/knob extends from an edge.
///
/// For internal edges, this determines whether the connector
/// protrudes toward one piece or the other. For border edges,
/// this field is meaningless and defaults to `In`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TabDirection {
    In,
    Out,
}

/// A single edge in the puzzle grid.
///
/// Edges are shared between adjacent pieces. Border edges lie
/// on the puzzle boundary; internal edges connect two pieces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// Start coordinate in mm.
    pub start: Point,
    /// End coordinate in mm.
    pub end: Point,
    /// True for edges on the puzzle boundary.
    pub is_border: bool,
    /// Tab direction (In or Out). Meaningless for borders, defaults to In.
    pub direction: TabDirection,
    /// Bezier curves for the connector shape. None until connector generation.
    pub connector: Option<Vec<CubicBez>>,
    /// Params used to generate `connector`. Stored so the connector can be
    /// regenerated at a different length when the edge is shortened by a
    /// custom border clipping — e.g., boundary-aware puzzles re-fit the
    /// knob onto the inside-boundary portion of the cell edge.
    #[serde(default)]
    pub connector_params: Option<EdgeParams>,
}

impl Edge {
    /// Compute the Euclidean length of this edge in mm.
    pub fn length(&self) -> f64 {
        let diff = self.end - self.start;
        diff.hypot()
    }
}

/// Parameters passed to a [`ConnectorGenerator`](super::ConnectorGenerator)
/// for generating connector shapes on internal edges.
///
/// The base profile and neck taper come from `classic_connector.rs`.
/// `knob_scale` adjusts its size, while `offset` moves it along the cut.
/// Keeping both here lets collision handling preserve the chosen style.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeParams {
    /// Edge length in mm.
    pub length: f64,
    /// Perpendicular span in mm: distance between adjacent piece
    /// centers. Used with `length` to bound knob protrusion via
    /// `min(length, cross_length)` so knobs don't overshoot into a
    /// neighbor's interior.
    pub cross_length: f64,
    /// Tab direction (In or Out).
    pub direction: TabDirection,
    /// Signed offset (mm) along the edge from the centered position.
    /// Positive shifts toward `end`, negative toward `start`. Used by
    /// the collision-resolution slide pass to push two colliding
    /// knobs along their respective edges. Defaults to `0.0` (knob
    /// centered on the edge).
    #[serde(default)]
    pub offset: f64,
    /// Uniform knob width/height multiplier. Kept in the parameters so
    /// collision resolution and regenerated variants use the same profile.
    #[serde(default = "default_knob_scale")]
    pub knob_scale: f64,
}

fn default_knob_scale() -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_length_horizontal() {
        let edge = Edge {
            start: Point::new(0.0, 0.0),
            end: Point::new(100.0, 0.0),
            is_border: false,
            direction: TabDirection::Out,
            connector: None,
            connector_params: None,
        };
        assert!((edge.length() - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_edge_length_vertical() {
        let edge = Edge {
            start: Point::new(10.0, 20.0),
            end: Point::new(10.0, 70.0),
            is_border: true,
            direction: TabDirection::In,
            connector: None,
            connector_params: None,
        };
        assert!((edge.length() - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_edge_length_diagonal() {
        let edge = Edge {
            start: Point::new(0.0, 0.0),
            end: Point::new(3.0, 4.0),
            is_border: false,
            direction: TabDirection::In,
            connector: None,
            connector_params: None,
        };
        assert!((edge.length() - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_edge_length_zero() {
        let edge = Edge {
            start: Point::new(5.0, 5.0),
            end: Point::new(5.0, 5.0),
            is_border: false,
            direction: TabDirection::In,
            connector: None,
            connector_params: None,
        };
        assert!((edge.length()).abs() < 1e-10);
    }

    #[test]
    fn test_tab_direction_serialize_deserialize() {
        let dir = TabDirection::Out;
        let json = serde_json::to_string(&dir).unwrap();
        assert_eq!(json, "\"Out\"");

        let deserialized: TabDirection = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TabDirection::Out);

        let dir_in = TabDirection::In;
        let json_in = serde_json::to_string(&dir_in).unwrap();
        assert_eq!(json_in, "\"In\"");

        let deserialized_in: TabDirection = serde_json::from_str(&json_in).unwrap();
        assert_eq!(deserialized_in, TabDirection::In);
    }

    #[test]
    fn test_edge_serialize_deserialize() {
        let edge = Edge {
            start: Point::new(0.0, 0.0),
            end: Point::new(50.0, 0.0),
            is_border: false,
            direction: TabDirection::Out,
            connector: None,
            connector_params: None,
        };
        let json = serde_json::to_string(&edge).unwrap();
        let deserialized: Edge = serde_json::from_str(&json).unwrap();
        assert!((deserialized.length() - 50.0).abs() < 1e-10);
        assert_eq!(deserialized.direction, TabDirection::Out);
        assert!(!deserialized.is_border);
        assert!(deserialized.connector.is_none());
    }
}
