use serde::{Deserialize, Serialize};

/// Piece layout algorithm. CVT and Poisson share the Voronoi pipeline;
/// Classic uses a separate rectangular mesh with four-sided pieces.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CellAlgorithm {
    /// Random rejection-scatter + Lloyd relaxation. Produces fully
    /// centroidal Voronoi cells. Slow at large piece counts due to
    /// the iterative Lloyd loop. The historical default.
    #[serde(rename = "cvt")]
    Cvt,
    /// Bridson's Poisson disc sampling. One-pass, O(N), no
    /// relaxation. Cells are well-spaced and roughly hexagonal but
    /// not strictly centroidal — visually ~85% of the way to CVT
    /// quality with a fraction of the compute cost.
    #[serde(rename = "poisson")]
    Poisson,
    /// Traditional four-sided pieces in a rectangular grid.
    #[serde(rename = "classic")]
    Classic,
}

impl Default for CellAlgorithm {
    fn default() -> Self {
        Self::Cvt
    }
}

/// Unit system for puzzle dimensions.
///
/// All internal computation uses millimeters. This enum allows
/// accepting user input in either unit and converting at the boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Unit {
    Millimeters,
    Inches,
}

impl Unit {
    /// Convert a value in this unit to millimeters.
    pub fn to_mm(&self, value: f64) -> f64 {
        match self {
            Unit::Millimeters => value,
            Unit::Inches => value * 25.4,
        }
    }

    /// Convert a value in millimeters to this unit.
    pub fn from_mm(&self, value_mm: f64) -> f64 {
        match self {
            Unit::Millimeters => value_mm,
            Unit::Inches => value_mm / 25.4,
        }
    }
}

/// Top-level puzzle configuration.
///
/// All dimensions are stored in millimeters internally. Voronoi layouts
/// support shaped borders and whimsies. Classic layouts use a rectangular
/// grid, rounding the target count to a complete grid of near-square cells.
///
/// Knob shape is fully determined by constants in
/// [`crate::classic_connector`]. If an edge would host a knob whose neck
/// falls below `3 mm`, the connector is skipped and that edge renders
/// as a straight line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PuzzleConfig {
    /// Target piece count. Classic rounds this to a complete grid.
    #[serde(default = "default_piece_count", alias = "piece_count")]
    pub piece_count: u32,
    /// Puzzle width in mm (after unit conversion).
    pub width: f64,
    /// Puzzle height in mm (after unit conversion).
    pub height: f64,
    /// Display/input unit.
    pub unit: Unit,
    /// User seed string (empty = auto-generate).
    pub seed: String,
    /// Optional border shape name (`"rectangle"`, `"heart"`, `"star"`, …).
    /// `None` or `"rectangle"` produces a rectangular boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_shape: Option<String>,
    /// When `true`, skip knob generation on every internal edge —
    /// pieces render as straight-cut polygons. Useful for inspecting
    /// the raw CVT tessellation (sliver detection, boundary clipping)
    /// without knob geometry on top.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_knobs: bool,
    /// When `true`, the puzzle's outer boundary (the silhouette
    /// itself) is rebuilt with classic knob bumps along its length so
    /// edge pieces look like interior pieces. Default is `false` —
    /// the silhouette stays smooth and edge pieces have one or more
    /// flat sides.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub knob_outer_boundary: bool,
    /// Figural pop-out pieces placed inside the border. Each whimsy is
    /// subtracted from the outer boundary before CVT runs — surrounding
    /// pieces hug the whimsy contour — and the whimsy itself is added
    /// as a separate piece (or, when `subdivisions > 0`, as a nested
    /// CVT of that many sub-pieces).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whimsies: Vec<WhimsyPlacement>,
    /// Cell-generation algorithm. Defaults to CVT for backward
    /// compatibility with existing URLs.
    #[serde(default)]
    pub cell_algorithm: CellAlgorithm,
    /// Number of Lloyd "polish" iterations to run after Bridson's
    /// Poisson disc seeding. Each polish iteration runs a single
    /// Lloyd relaxation step (move each seed toward its clipped-cell
    /// centroid), nudging cells closer to equal-size centroidal
    /// shapes. `0` means raw Bridson output — fastest but with
    /// visible cell-size variation. `3` is a good default — most of
    /// the size disparity disappears, still a fraction of full
    /// CVT-from-random cost. Range 0–10 (clamped). Ignored when
    /// `cell_algorithm == Cvt`.
    #[serde(default = "default_poisson_polish_iterations")]
    pub poisson_polish_iterations: u32,
    /// Classic knob size relative to the original shape (0.5–1.25).
    #[serde(default = "default_classic_knob_size")]
    pub classic_knob_size: f64,
    /// Classic corner jitter and side curvature, from regular (0) to organic (1).
    #[serde(default)]
    pub classic_grid_irregularity: f64,
    /// Seeded variation of knob positions along their sides (0–1).
    #[serde(default)]
    pub classic_knob_randomness: f64,
}

fn default_classic_knob_size() -> f64 {
    1.0
}

fn default_poisson_polish_iterations() -> u32 {
    3
}

/// One figural whimsy placed inside the puzzle boundary. The whimsy is
/// the pre-rotation bounding-box size (`width` × `height`) rotated
/// `rotation` degrees about `(center_x, center_y)`. Coordinates are in
/// mm, relative to the puzzle's top-left corner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhimsyPlacement {
    /// Shape name resolvable by the boundary/whimsy shape library
    /// (`"heart"`, `"star"`, `"circle"`, …).
    pub shape: String,
    /// Center x in mm, relative to puzzle top-left.
    pub center_x: f64,
    /// Center y in mm, relative to puzzle top-left.
    pub center_y: f64,
    /// Whimsy bounding-box width (pre-rotation), mm.
    pub width: f64,
    /// Whimsy bounding-box height (pre-rotation), mm.
    pub height: f64,
    /// Rotation in degrees, applied about the whimsy center.
    #[serde(default)]
    pub rotation: f64,
    /// Number of sub-pieces to tile inside the whimsy via a nested CVT.
    /// `0` means the whimsy is one solid pop-out piece with no internal
    /// knobbed edges — its outer contour is the only cut line.
    #[serde(default)]
    pub subdivisions: u32,
}

fn default_piece_count() -> u32 {
    48
}

/// Minimum acceptable piece count. Below 2 there's nothing for Lloyd
/// relaxation to optimise; CVT itself needs ≥2 seeds.
pub const MIN_PIECE_COUNT: u32 = 2;
/// Upper sanity bound on piece count — mostly a belt-and-braces check
/// against accidentally over-large requests that would stall Lloyd.
pub const MAX_PIECE_COUNT: u32 = 5_000;

impl Default for PuzzleConfig {
    fn default() -> Self {
        Self {
            piece_count: default_piece_count(),
            width: 297.0,
            height: 210.0,
            unit: Unit::Millimeters,
            seed: String::new(),
            border_shape: None,
            disable_knobs: false,
            knob_outer_boundary: false,
            whimsies: Vec::new(),
            cell_algorithm: CellAlgorithm::default(),
            poisson_polish_iterations: default_poisson_polish_iterations(),
            classic_knob_size: default_classic_knob_size(),
            classic_grid_irregularity: 0.0,
            classic_knob_randomness: 0.0,
        }
    }
}

impl PuzzleConfig {
    /// Validate all configuration bounds. Returns the first error found.
    pub fn validate(&self) -> Result<(), String> {
        if self.piece_count < MIN_PIECE_COUNT || self.piece_count > MAX_PIECE_COUNT {
            return Err(format!(
                "piece_count must be between {MIN_PIECE_COUNT} and {MAX_PIECE_COUNT}, got {}",
                self.piece_count
            ));
        }
        if !self.width.is_finite() || self.width <= 0.0 {
            return Err(format!("width must be positive, got {}", self.width));
        }
        if !self.height.is_finite() || self.height <= 0.0 {
            return Err(format!("height must be positive, got {}", self.height));
        }
        for (i, w) in self.whimsies.iter().enumerate() {
            w.validate().map_err(|e| format!("whimsy[{i}]: {e}"))?;
        }
        if self.cell_algorithm == CellAlgorithm::Classic {
            for (name, value, min, max) in [
                ("classic_knob_size", self.classic_knob_size, 0.5, 1.25),
                ("classic_grid_irregularity", self.classic_grid_irregularity, 0.0, 1.0),
                ("classic_knob_randomness", self.classic_knob_randomness, 0.0, 1.0),
            ] {
                if !value.is_finite() || value < min || value > max {
                    return Err(format!("{name} must be between {min} and {max}, got {value}"));
                }
            }
            if !matches!(self.border_shape.as_deref(), None | Some("") | Some("rectangle")) {
                return Err("Classic grid requires a rectangular border; use CVT or Poisson for shaped borders".into());
            }
            if !self.whimsies.is_empty() {
                return Err("Classic grid does not support whimsies; use CVT or Poisson for whimsy pieces".into());
            }
        }
        Ok(())
    }

}

impl WhimsyPlacement {
    /// Validate the whimsy's own bounds. Does not check that the whimsy
    /// actually fits inside the puzzle — that's enforced at placement
    /// time in the frontend.
    pub fn validate(&self) -> Result<(), String> {
        if self.shape.trim().is_empty() {
            return Err("shape must not be empty".to_string());
        }
        if self.width <= 0.0 || self.height <= 0.0 {
            return Err(format!(
                "width/height must be positive, got {} × {}",
                self.width, self.height
            ));
        }
        Ok(())
    }
}

impl PuzzleConfig {
    /// Construct a PuzzleConfig from user input, converting units to mm.
    pub fn from_input(
        piece_count: u32,
        width: f64,
        height: f64,
        unit: Unit,
        seed: String,
    ) -> Result<Self, String> {
        let config = Self {
            piece_count,
            width: unit.to_mm(width),
            height: unit.to_mm(height),
            unit,
            seed,
            border_shape: None,
            disable_knobs: false,
            knob_outer_boundary: false,
            whimsies: Vec::new(),
            cell_algorithm: CellAlgorithm::default(),
            poisson_polish_iterations: default_poisson_polish_iterations(),
            classic_knob_size: default_classic_knob_size(),
            classic_grid_irregularity: 0.0,
            classic_knob_randomness: 0.0,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unit_roundtrip() {
        let original = 12.5;
        let mm = Unit::Inches.to_mm(original);
        let back = Unit::Inches.from_mm(mm);
        assert!((back - original).abs() < 1e-10);
    }

    #[test]
    fn test_default_config_is_valid() {
        let config = PuzzleConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.piece_count, 48);
    }

    #[test]
    fn test_validate_piece_count_too_low() {
        let config = PuzzleConfig {
            piece_count: 1,
            ..PuzzleConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_piece_count_too_high() {
        let config = PuzzleConfig {
            piece_count: MAX_PIECE_COUNT + 1,
            ..PuzzleConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_negative_width() {
        let config = PuzzleConfig {
            width: -10.0,
            ..PuzzleConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_from_input_inches_converts() {
        let config = PuzzleConfig::from_input(48, 10.0, 8.0, Unit::Inches, "t".into()).unwrap();
        assert!((config.width - 254.0).abs() < 1e-10);
        assert!((config.height - 203.2).abs() < 1e-10);
    }
}
