//! Traditional rectangular jigsaw mesh. Each interior cut is stored once
//! and shared by its two neighboring pieces, ensuring matching connectors.

use kurbo::{BezPath, CubicBez, Point, Shape};
use rand::RngExt;

use crate::{
    CellAlgorithm, ClassicKnobConnector, ConnectorGenerator, EdgeParams, LayoutEdge, LayoutPiece,
    MAX_PIECE_COUNT, PuzzleConfig, PuzzleLayout, TabDirection, create_rng, edge_transform,
    rect_path,
};

/// Choose columns and rows, balancing closeness to the requested count
/// with square cells. Favor count accuracy, without forcing prime counts
/// into a single long strip. The resulting count stays within config bounds.
pub fn classic_grid_dimensions(target: u32, width: f64, height: f64) -> (usize, usize) {
    let target = target.clamp(2, MAX_PIECE_COUNT) as usize;
    let max = MAX_PIECE_COUNT as usize;
    let mut best = (target, 1);
    let mut best_score = f64::INFINITY;
    for rows in 1..=target {
        let columns = ((target as f64 / rows as f64).round() as usize).clamp(1, max / rows);
        let count = rows * columns;
        if count < 2 {
            continue;
        }
        let count_error = (count as f64 / target as f64).ln().abs();
        let cell_aspect = (width / columns as f64) / (height / rows as f64);
        let score = 6.0 * count_error + cell_aspect.ln().abs();
        if score < best_score {
            best_score = score;
            best = (columns, rows);
        }
    }
    best
}

/// Build a complete rectangular mesh, without Voronoi relaxation or
/// merging (which would change the four-sided topology).
pub fn build_classic_layout(config: &PuzzleConfig) -> Result<PuzzleLayout, String> {
    config.validate()?;
    if config.cell_algorithm != CellAlgorithm::Classic {
        return Err("Classic grid requires the classic algorithm".into());
    }
    let (columns, rows) = classic_grid_dimensions(config.piece_count, config.width, config.height);
    let cell_width = config.width / columns as f64;
    let cell_height = config.height / rows as f64;
    let mut layout = PuzzleLayout {
        width: config.width,
        height: config.height,
        outer_boundary: rect_path(config.width, config.height),
        edges: Vec::new(),
        pieces: Vec::with_capacity(columns * rows),
        knob_base_cap: 0.0,
    };

    // Jitter shared vertices once, keeping the frame straight. A displacement
    // of at most 18% per axis preserves convex, non-crossing quadrilaterals.
    let mut grid_rng = create_rng(&format!("{}-grid-vertices", config.seed));
    let mut vertices = Vec::with_capacity((columns + 1) * (rows + 1));
    for row in 0..=rows {
        for column in 0..=columns {
            let mut point = Point::new(
                config.width * column as f64 / columns as f64,
                config.height * row as f64 / rows as f64,
            );
            if config.classic_grid_irregularity > 0.0 {
                let dx = grid_rng.random_range(-1.0..1.0);
                let dy = grid_rng.random_range(-1.0..1.0);
                if column > 0 && column < columns {
                    point.x += dx * cell_width * 0.18 * config.classic_grid_irregularity;
                }
                if row > 0 && row < rows {
                    point.y += dy * cell_height * 0.18 * config.classic_grid_irregularity;
                }
            }
            vertices.push(point);
        }
    }
    let vertex = |column: usize, row: usize| vertices[row * (columns + 1) + column];
    for row in 0..rows {
        for column in 0..columns {
            let mut outline = BezPath::new();
            outline.move_to(vertex(column, row));
            outline.line_to(vertex(column + 1, row));
            outline.line_to(vertex(column + 1, row + 1));
            outline.line_to(vertex(column, row + 1));
            outline.close_path();
            layout.pieces.push(LayoutPiece {
                id: row * columns + column,
                center: if config.classic_grid_irregularity == 0.0 {
                    Point::new(
                        (column as f64 + 0.5) * cell_width,
                        (row as f64 + 0.5) * cell_height,
                    )
                } else {
                    let points = [
                        vertex(column, row),
                        vertex(column + 1, row),
                        vertex(column + 1, row + 1),
                        vertex(column, row + 1),
                    ];
                    Point::new(
                        points.iter().map(|p| p.x).sum::<f64>() / 4.0,
                        points.iter().map(|p| p.y).sum::<f64>() / 4.0,
                    )
                },
                edge_indices: Vec::with_capacity(4),
                outline: Some(outline),
            });
        }
    }

    let mut direction_rng = create_rng(&format!("{}-grid-directions", config.seed));
    let mut add_edge = |start, end, a: usize, b: usize| {
        let index = layout.edges.len();
        layout.edges.push(LayoutEdge {
            start,
            end,
            direction: if direction_rng.random_bool(0.5) {
                TabDirection::Out
            } else {
                TabDirection::In
            },
            connector: None,
            connector_params: None,
            pieces: (a, b),
        });
        layout.pieces[a].edge_indices.push(index);
        layout.pieces[b].edge_indices.push(index);
    };
    for row in 0..rows {
        for column in 1..columns {
            let right = row * columns + column;
            add_edge(
                vertex(column, row),
                vertex(column, row + 1),
                right - 1,
                right,
            );
        }
    }
    for row in 1..rows {
        for column in 0..columns {
            let bottom = row * columns + column;
            add_edge(
                vertex(column, row),
                vertex(column + 1, row),
                bottom - columns,
                bottom,
            );
        }
    }

    let original_style = config.classic_knob_size == 1.0
        && config.classic_knob_randomness == 0.0
        && config.classic_grid_irregularity == 0.0;
    if original_style && !config.disable_knobs {
        layout.generate_connectors(&ClassicKnobConnector, &config.seed, 0.0);
        layout.compute_and_cap_avg_knob_base(&ClassicKnobConnector);
        // Centered connectors fit in disjoint wedges of each rectangular
        // cell. The Voronoi collision pass uses conservative boxes that
        // would unnecessarily slide and shrink these classic knobs.
    } else if !original_style {
        let mut position_rng = create_rng(&format!("{}-grid-knob-positions", config.seed));
        let mut curve_rng = create_rng(&format!("{}-grid-curvature", config.seed));
        for edge in &mut layout.edges {
            let centers = [
                layout.pieces[edge.pieces.0].center,
                layout.pieces[edge.pieces.1].center,
            ];
            let offset = position_rng.random_range(-1.0..1.0)
                * edge.length()
                * 0.18
                * config.classic_knob_randomness;
            let bend = curve_rng.random_range(-1.0..1.0)
                * cell_width.min(cell_height)
                * 0.06
                * config.classic_grid_irregularity;
            let (curves, params) = styled_cut(
                edge.start,
                edge.end,
                centers,
                edge.direction,
                offset,
                bend,
                config,
            );
            edge.connector = curves;
            edge.connector_params = params;
        }
    }
    if config.knob_outer_boundary && original_style && !config.disable_knobs {
        layout.knob_outer_boundary(&ClassicKnobConnector, &config.seed);
    } else if config.knob_outer_boundary && !config.disable_knobs {
        // Build each frame side from the same shared grid vertices. Its
        // inward half occupies only that border piece's triangle, so it
        // cannot collide with any of that piece's other cuts.
        let mut boundary_edges = Vec::new();
        for column in 0..columns {
            boundary_edges.push((vertex(column, 0), vertex(column + 1, 0), column));
        }
        for row in 0..rows {
            boundary_edges.push((
                vertex(columns, row),
                vertex(columns, row + 1),
                row * columns + columns - 1,
            ));
        }
        for column in (0..columns).rev() {
            boundary_edges.push((
                vertex(column + 1, rows),
                vertex(column, rows),
                (rows - 1) * columns + column,
            ));
        }
        for row in (0..rows).rev() {
            boundary_edges.push((vertex(0, row + 1), vertex(0, row), row * columns));
        }
        let mut boundary = BezPath::new();
        boundary.move_to(vertex(0, 0));
        let mut rng = create_rng(&format!("{}-grid-outer-style", config.seed));
        for (start, end, owner) in boundary_edges {
            let center = layout.pieces[owner].center;
            let axis = (end - start) / start.distance(end);
            let foot = start + axis * (center - start).dot(axis);
            let reflected = foot + (foot - center);
            let direction = if rng.random_bool(0.5) {
                TabDirection::In
            } else {
                TabDirection::Out
            };
            let offset = rng.random_range(-1.0..1.0)
                * start.distance(end)
                * 0.18
                * config.classic_knob_randomness;
            let (curves, _) = styled_cut(
                start,
                end,
                [center, reflected],
                direction,
                offset,
                0.0,
                config,
            );
            let transform = edge_transform(start, end);
            if let Some(curves) = curves {
                for curve in curves {
                    boundary.curve_to(
                        transform * curve.p1,
                        transform * curve.p2,
                        transform * curve.p3,
                    );
                }
            } else {
                boundary.line_to(end);
            }
        }
        boundary.close_path();
        layout.outer_boundary = boundary;
    }
    Ok(layout)
}

/// Fit a styled cut into the two adjacent center-to-side triangles. These
/// triangles partition each convex piece, so curves in different sides'
/// triangles cannot cross, even with maximum jitter, size, and offset.
fn styled_cut(
    start: Point,
    end: Point,
    centers: [Point; 2],
    direction: TabDirection,
    offset: f64,
    bend: f64,
    config: &PuzzleConfig,
) -> (Option<Vec<CubicBez>>, Option<EdgeParams>) {
    if config.disable_knobs && bend == 0.0 {
        return (None, None);
    }
    let length = start.distance(end);
    let cross_length = centers[0].distance(centers[1]);
    let mut rng = create_rng("classic-shape"); // Shape generator itself uses no randomness.
    for attempt in 0..64 {
        let fit = 0.9_f64.powi(attempt);
        let size = config.classic_knob_size * fit;
        let params = EdgeParams {
            length,
            cross_length,
            direction,
            offset,
        };
        let mut curves = if config.disable_knobs {
            vec![CubicBez::new(
                Point::ZERO,
                Point::new(length / 3.0, 0.0),
                Point::new(length * 2.0 / 3.0, 0.0),
                Point::new(length, 0.0),
            )]
        } else {
            let scaled = EdgeParams {
                length: length * size,
                cross_length: cross_length * size,
                direction,
                offset: 0.0,
            };
            let mut curves = ClassicKnobConnector.generate(&scaled, &mut rng);
            let shift = (length - scaled.length) * 0.5 + offset;
            for curve in &mut curves {
                curve.p0.x += shift;
                curve.p1.x += shift;
                curve.p2.x += shift;
                curve.p3.x += shift;
            }
            curves[0].p0 = Point::ZERO;
            curves.last_mut().unwrap().p3 = Point::new(length, 0.0);
            curves
        };
        for curve in &mut curves {
            for point in [&mut curve.p0, &mut curve.p1, &mut curve.p2, &mut curve.p3] {
                let t = point.x / length;
                point.y += 4.0 * bend * fit * t * (1.0 - t);
            }
        }
        if cut_fits_wedges(&curves, start, end, centers) {
            return (
                Some(curves),
                if config.disable_knobs {
                    None
                } else {
                    Some(params)
                },
            );
        }
    }
    // Degenerate geometry fallback: the shared straight segment is always
    // valid and preserves all four sides and the target piece count.
    (None, None)
}

fn cut_fits_wedges(curves: &[CubicBez], start: Point, end: Point, centers: [Point; 2]) -> bool {
    let polygon = [start, centers[0], end, centers[1]];
    let orientation = (polygon[1] - polygon[0])
        .cross(polygon[2] - polygon[0])
        .signum();
    let transform = edge_transform(start, end);
    let tolerance = start.distance(end).powi(2) * 1e-10;
    for index in 0..4 {
        let a = polygon[index];
        let side = polygon[(index + 1) % 4] - a;
        let signed_distance =
            |p: Point| Point::new(orientation * side.cross(transform * p - a), 0.0);
        for curve in curves {
            let projected = CubicBez::new(
                signed_distance(curve.p0),
                signed_distance(curve.p1),
                signed_distance(curve.p2),
                signed_distance(curve.p3),
            );
            if projected.bounding_box().x0 < -tolerance {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurbo::{CubicBez, ParamCurve, PathEl, Shape};

    fn config() -> PuzzleConfig {
        PuzzleConfig {
            cell_algorithm: CellAlgorithm::Classic,
            seed: "classic-test".into(),
            ..PuzzleConfig::default()
        }
    }

    #[test]
    fn style_controls_change_size_and_placement_independently() {
        let mut config = config();
        let original = build_classic_layout(&config).unwrap();
        config.classic_knob_size = 0.5;
        let small = build_classic_layout(&config).unwrap();
        config.classic_knob_size = 1.25;
        let large = build_classic_layout(&config).unwrap();
        let height =
            |layout: &PuzzleLayout| layout.edges[0].connector.as_ref().unwrap()[2].p3.y.abs();
        assert!((height(&small) / height(&original) - 0.5).abs() < 1e-8);
        assert!(height(&large) > height(&original));
        config.classic_knob_size = 1.0;
        config.classic_knob_randomness = 1.0;
        let shifted = build_classic_layout(&config).unwrap();
        assert!(
            shifted
                .edges
                .iter()
                .any(|e| e.connector_params.as_ref().unwrap().offset.abs() > e.length() * 0.1)
        );
        for (original, shifted) in original.edges.iter().zip(&shifted.edges) {
            assert_eq!(original.start, shifted.start);
            assert_eq!(original.end, shifted.end);
            assert_eq!(original.direction, shifted.direction);
        }
        assert_eq!(
            crate::layout_generate_svg(&shifted),
            crate::layout_generate_svg(&build_classic_layout(&config).unwrap())
        );
    }

    #[test]
    fn irregular_grid_has_convex_four_sided_cells_and_curves_without_knobs() {
        let mut config = config();
        config.classic_grid_irregularity = 1.0;
        config.disable_knobs = true;
        let layout = build_classic_layout(&config).unwrap();
        assert_eq!(layout.pieces.len(), 48);
        let mut area = 0.0;
        for piece in &layout.pieces {
            let outline = piece.outline.as_ref().unwrap();
            assert_eq!(outline.elements().len(), 5);
            assert!(outline.area() > 0.0);
            assert_ne!(outline.winding(piece.center), 0);
            area += outline.area();
        }
        assert!((area - config.width * config.height).abs() < 1e-6);
        assert!(
            layout
                .edges
                .iter()
                .any(|e| e.start.x != e.end.x && e.start.y != e.end.y)
        );
        assert!(layout.edges.iter().all(|e| e.connector_params.is_none()));
        assert!(layout.edges.iter().any(|e| {
            e.connector
                .as_ref()
                .is_some_and(|c| c.len() == 1 && c[0].p1.y.abs() > 0.01)
        }));
    }

    #[test]
    fn maximum_variations_keep_cuts_separate_and_all_tabs_present() {
        // Test complete cuts against neighboring cuts, independently of
        // the fitting code's analytic containment test.
        fn segments_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
            let cross = (b - a).cross(d - c);
            if cross.abs() < 1e-10 {
                return false;
            }
            let t = (c - a).cross(d - c) / cross;
            let u = (c - a).cross(b - a) / cross;
            (1e-8..1.0 - 1e-8).contains(&t) && (1e-8..1.0 - 1e-8).contains(&u)
        }
        for (width, height, count) in [
            (297.0, 210.0, 48),
            (210.0, 297.0, 47),
            (10.0, 1000.0, 12),
            (1000.0, 1.0, 2),
        ] {
            for seed in 0..6 {
                let mut config = config();
                config.width = width;
                config.height = height;
                config.piece_count = count;
                config.seed = format!("style-stress-{seed}");
                config.classic_grid_irregularity = 1.0;
                config.classic_knob_randomness = 1.0;
                config.classic_knob_size = 1.25;
                let layout = build_classic_layout(&config).unwrap();
                let cuts: Vec<Vec<Point>> = layout
                    .edges
                    .iter()
                    .map(|edge| {
                        let transform = edge_transform(edge.start, edge.end);
                        let curves = edge.connector.as_ref().expect("a tab fits every side");
                        assert_eq!(curves.len(), 5);
                        assert!(curves[0].p0.distance(Point::ZERO) < 1e-8);
                        assert!(
                            curves
                                .last()
                                .unwrap()
                                .p3
                                .distance(Point::new(edge.length(), 0.0))
                                < 1e-8
                        );
                        curves
                            .iter()
                            .flat_map(|curve| {
                                (0..=16).map(move |i| transform * curve.eval(i as f64 / 16.0))
                            })
                            .collect()
                    })
                    .collect();
                for piece in &layout.pieces {
                    for (i, &a) in piece.edge_indices.iter().enumerate() {
                        for &b in &piece.edge_indices[i + 1..] {
                            for line_a in cuts[a].windows(2) {
                                for line_b in cuts[b].windows(2) {
                                    assert!(
                                        !segments_cross(line_a[0], line_a[1], line_b[0], line_b[1]),
                                        "cuts cross for seed {seed}"
                                    );
                                }
                            }
                        }
                    }
                }
                let area: f64 = layout
                    .pieces
                    .iter()
                    .map(|p| p.outline.as_ref().unwrap().area())
                    .sum();
                assert!((area - width * height).abs() < 1e-6);
                config.knob_outer_boundary = true;
                let border = build_classic_layout(&config).unwrap();
                assert!(
                    border
                        .outer_boundary
                        .elements()
                        .iter()
                        .any(|el| matches!(el, PathEl::CurveTo(..)))
                );
                assert_eq!(border.pieces.len(), layout.pieces.len());
            }
        }
    }

    #[test]
    fn invalid_classic_style_values_are_rejected() {
        let mut config = config();
        for value in [f64::NAN, f64::INFINITY, -1.0, 1.26] {
            config.classic_knob_size = value;
            assert!(build_classic_layout(&config).is_err());
        }
        config.classic_knob_size = 1.0;
        config.classic_grid_irregularity = 1.01;
        assert!(build_classic_layout(&config).is_err());
        config.classic_grid_irregularity = 0.0;
        config.classic_knob_randomness = -0.01;
        assert!(build_classic_layout(&config).is_err());
    }

    #[test]
    fn four_sided_cells_tile_the_rectangle_and_share_every_cut() {
        let config = config();
        let layout = build_classic_layout(&config).unwrap();
        let (columns, rows) = classic_grid_dimensions(48, config.width, config.height);
        assert_eq!((columns, rows), (8, 6));
        assert_eq!(layout.pieces.len(), 48);
        assert_eq!(
            layout.edges.len(),
            rows * (columns - 1) + columns * (rows - 1)
        );
        let mut area = 0.0;
        for piece in &layout.pieces {
            let outline = piece.outline.as_ref().unwrap();
            assert_eq!(outline.elements().len(), 5);
            assert!(matches!(outline.elements()[4], PathEl::ClosePath));
            area += outline.area();
            let row = piece.id / columns;
            let column = piece.id % columns;
            let border_sides = usize::from(row == 0)
                + usize::from(row == rows - 1)
                + usize::from(column == 0)
                + usize::from(column == columns - 1);
            assert_eq!(piece.edge_indices.len() + border_sides, 4);
        }
        assert!((area - config.width * config.height).abs() < 1e-6);
        for (index, edge) in layout.edges.iter().enumerate() {
            let owners: Vec<_> = layout
                .pieces
                .iter()
                .filter(|p| p.edge_indices.contains(&index))
                .map(|p| p.id)
                .collect();
            assert_eq!(owners, vec![edge.pieces.0, edge.pieces.1]);
            let curves = edge
                .connector
                .as_ref()
                .expect("every default grid edge has a knob");
            assert!(curves[0].p0.distance(Point::ZERO) < 1e-8);
            assert!(
                curves
                    .last()
                    .unwrap()
                    .p3
                    .distance(Point::new(edge.length(), 0.0))
                    < 1e-8
            );
        }
    }

    #[test]
    fn prime_targets_use_a_nearby_grid_and_respect_count_bounds() {
        for (target, width, height) in [
            (47, 297.0, 210.0),
            (101, 200.0, 200.0),
            (5000, 297.0, 210.0),
            (2, 1.0, 1000.0),
        ] {
            let (columns, rows) = classic_grid_dimensions(target, width, height);
            assert!((2..=MAX_PIECE_COUNT as usize).contains(&(columns * rows)));
            if target == 47 || target == 101 {
                assert!(columns > 1 && rows > 1);
                assert!((columns * rows).abs_diff(target as usize) <= 4);
            }
        }
        let (columns, rows) = classic_grid_dimensions(48, 210.0, 297.0);
        assert_eq!((columns, rows), (6, 8));
    }

    #[test]
    fn seed_controls_connectors_deterministically_without_changing_the_grid() {
        let mut config = config();
        let a = build_classic_layout(&config).unwrap();
        let b = build_classic_layout(&config).unwrap();
        assert_eq!(
            crate::layout_generate_svg(&a),
            crate::layout_generate_svg(&b)
        );
        config.seed = "another-seed".into();
        let c = build_classic_layout(&config).unwrap();
        assert_ne!(
            crate::layout_generate_svg(&a),
            crate::layout_generate_svg(&c)
        );
        assert_eq!(a.pieces.len(), c.pieces.len());
        assert!(
            a.edges
                .iter()
                .zip(&c.edges)
                .all(|(a, c)| a.start == c.start && a.end == c.end)
        );
    }

    #[test]
    fn knobs_can_be_disabled_without_changing_the_four_sided_mesh() {
        let mut config = config();
        config.disable_knobs = true;
        let layout = build_classic_layout(&config).unwrap();
        assert_eq!(layout.pieces.len(), 48);
        assert!(layout.edges.iter().all(|e| e.connector.is_none()));
        assert!(!crate::layout_generate_svg(&layout).contains('C'));
    }

    #[test]
    fn centered_connectors_stay_in_disjoint_cell_wedges() {
        for (width, height) in [
            (297.0, 210.0),
            (210.0, 297.0),
            (200.0, 200.0),
            (1000.0, 10.0),
        ] {
            let mut config = config();
            config.width = width;
            config.height = height;
            let layout = build_classic_layout(&config).unwrap();
            for edge in &layout.edges {
                let params = edge.connector_params.as_ref().unwrap();
                assert_eq!(params.offset, 0.0);
                // In normalized cell coordinates (u along the edge,
                // v into the cell), v <= u and v <= 1-u confine each
                // side's connector to its own triangle. Exact Bezier
                // extrema prove neighboring cuts cannot cross.
                let map = |p: Point| {
                    let u = p.x / params.length;
                    let v = p.y.abs() / params.cross_length;
                    Point::new(v - u, v + u - 1.0)
                };
                for curve in edge.connector.as_ref().unwrap() {
                    let bounds =
                        CubicBez::new(map(curve.p0), map(curve.p1), map(curve.p2), map(curve.p3))
                            .bounding_box();
                    assert!(
                        bounds.x1 <= 1e-10 && bounds.y1 <= 1e-10,
                        "connector leaves its cell wedge: {bounds:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn incompatible_shapes_and_whimsies_are_rejected() {
        let mut config = config();
        config.border_shape = Some("heart".into());
        assert!(
            build_classic_layout(&config)
                .unwrap_err()
                .contains("rectangular")
        );
        config.border_shape = None;
        config.whimsies.push(crate::WhimsyPlacement {
            shape: "circle".into(),
            center_x: 100.0,
            center_y: 100.0,
            width: 40.0,
            height: 40.0,
            rotation: 0.0,
            subdivisions: 0,
        });
        assert!(
            build_classic_layout(&config)
                .unwrap_err()
                .contains("whimsies")
        );
    }
}
