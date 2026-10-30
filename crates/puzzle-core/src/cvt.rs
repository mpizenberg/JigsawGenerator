//! Centroidal Voronoi Tessellation (CVT) layout builder.
//!
//! Produces a [`PuzzleLayout`] whose pieces are the Voronoi cells of
//! `piece_count` seed points scattered inside a closed boundary, after
//! Lloyd relaxation has driven the seeds toward a centroidal
//! arrangement (cells roughly uniform in area, convex, boundary-
//! respecting).
//!
//! The algorithm in outline:
//!
//! 1. Flatten the boundary once to a polygon at a fixed tolerance.
//! 2. Rejection-sample `piece_count` initial seed points uniformly
//!    inside the bounding rectangle, keeping only those inside the
//!    boundary polygon.
//! 3. For `lloyd_iterations` iterations: build the Voronoi diagram over
//!    the current seeds (using the `voronoice` crate), clip each cell
//!    to the boundary polygon via `linesweeper`, compute the centroid
//!    of the clipped polygon, move the seed to that centroid.
//! 4. Build one final Voronoi, clip every cell to the boundary, extract
//!    internal edges by finding shared polygon edges between neighbor
//!    cells, emit [`LayoutEdge`]s and [`LayoutPiece`]s with cell
//!    polygons cached as outlines.
//! 5. Generate connectors on every internal edge via
//!    [`PuzzleLayout::generate_connectors`].
//!
//! Determinism: two [`ChaCha8Rng`] streams are used —
//! `"{seed}-positions"` for the initial scatter and `"{seed}-connectors"`
//! (inside `generate_connectors`) for tab randomization. Lloyd itself is
//! a purely geometric fixed-point iteration, no RNG.

use kurbo::{BezPath, PathEl, Point, Shape, Vec2};
use rand::RngExt;
use voronoice::{BoundingBox, ClipBehavior, Point as VPoint, Voronoi, VoronoiBuilder};

use crate::classic_connector::{ClassicKnobConnector, NECK_OPENING_RATIO};
use crate::edge::TabDirection;
use crate::flat_boundary::{
    flatten_polygon, polygon_contains_indexed, BoundaryIndex,
};
use crate::layout::{LayoutEdge, LayoutPiece, PuzzleLayout};
use crate::masking::mask_intersection;
use crate::seed::create_rng;

// ─── Phase timing helpers ────────────────────────────────────────────
//
// In WASM builds these emit `[perf cvt] {phase}: {ms}` lines to the
// browser console; native builds compile to no-ops. Lets us see the
// per-phase breakdown of `build_cvt_layout` during interactive drags
// without dragging web-sys into native test runs.

#[cfg(target_arch = "wasm32")]
fn phase_now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}
#[cfg(not(target_arch = "wasm32"))]
fn phase_now() -> f64 {
    0.0
}

#[cfg(target_arch = "wasm32")]
fn phase_log(name: &str, ms: f64) {
    web_sys::console::log_1(&format!("[perf cvt]   {name}: {ms:.1} ms").into());
}
#[cfg(not(target_arch = "wasm32"))]
fn phase_log(_: &str, _: f64) {}

/// Default tolerance (mm) for flattening the boundary to a polygon.
/// Same order of magnitude as the previous boundary clipping path —
/// comfortably finer than any laser kerf, while keeping the flattened
/// line count bounded.
pub const DEFAULT_FLATTEN_TOLERANCE: f64 = 0.25;

/// Default Lloyd iteration count. Lloyd converges quickly in the
/// average-distance sense — most cell-center movement is in the
/// first ~10 iterations — but interior-cell *shape* keeps tightening
/// (closer to regular hexagons) for many more iterations, which in
/// turn flattens the edge-length and knob-size distributions. 20 is a
/// comfortable default: visually indistinguishable from 30 (relaxation
/// has long since plateaued by iteration 20), and 1.5× faster on the
/// dominant Lloyd loop cost.
pub const DEFAULT_LLOYD_ITERATIONS: usize = 20;

/// Adaptive Lloyd termination threshold (mm). Lloyd is a fixed-point
/// iteration; once max-per-seed displacement falls below this, the
/// remaining iterations are essentially no-ops. 0.1mm is well below
/// laser kerf and well below the smallest visually meaningful piece
/// size. Empirically, well-conditioned puzzles converge in 10–14
/// iterations rather than the full 20.
const LLOYD_CONVERGENCE_DELTA_MM: f64 = 0.1;

/// Default edge length (mm) below which two adjacent CVT pieces would
/// merge into one. `0.0` disables merging — short edges are kept as
/// piece boundaries but render without a knob (via
/// [`DEFAULT_MIN_KNOB_EDGE_LENGTH`]).
///
/// Callers who do want merging (e.g. a pipeline that treats thin tips
/// as a single piece) can set this explicitly on `CvtParams`; the
/// [`PuzzleLayout::merge_short_edges`] method is the implementation.
pub const DEFAULT_MERGE_EDGE_THRESHOLD: f64 = 0.0;

/// Minimum acceptable neck opening (mm). The knob's narrowest span.
/// Anything below this is either impossible to laser-cut cleanly or too
/// small to let a puzzle piece actually grip its neighbor.
pub const MIN_KNOB_NECK_OPENING_MM: f64 = 3.0;

/// Minimum interior-corner angle (degrees) below which a CVT cell is
/// treated as a fragile sliver and merged into its largest neighbor.
/// A well-behaved CVT cell has every corner comfortably above 60°;
/// a piece inheriting a heart tip or star point lands in the 25–40°
/// range. Threshold split the difference at 35° so genuine puzzle-tip
/// slivers get absorbed without sweeping up regular boundary cells.
pub const DEFAULT_MIN_PIECE_ANGLE_DEG: f64 = 35.0;

/// Maximum interior angle (degrees) at which a whimsy-convex corner is
/// treated as "sharp" and gets a pair of anchor seeds placed across
/// its outward axis. Covers heart bottom tips (~6°), star outer tips
/// (~36°), triangle/diamond corners (60°). Hexagon corners (120°) and
/// rounded-rect corners (90° but softened) are safely above the cutoff.
pub const DEFAULT_SHARP_CORNER_ANGLE_DEG: f64 = 60.0;

/// Fraction of the average piece linear dimension used as the axial
/// offset of each anchor seed from its sharp corner (along the outward
/// axis). Must stay well below half the typical inter-seed distance
/// so the anchor pair reliably owns the tip's cell instead of letting
/// a random seed slot in between anchor and tip.
pub const ANCHOR_OFFSET_FRACTION: f64 = 0.2;

/// Fraction of the average piece linear dimension used as the tangential
/// spread of each anchor seed from the corner axis. Smaller spread
/// keeps each anchor cell tight around its half of the tip.
pub const ANCHOR_SPREAD_FRACTION: f64 = 0.12;

/// Knob-clearance shell radius as a fraction of the layout's average
/// knob base (`min(length, cross_length)`). Scaling with the puzzle's
/// natural piece scale keeps the buffer visually proportional across
/// piece counts: a tight 1000-piece puzzle has a small absolute
/// clearance, a coarse 24-piece puzzle has a generous one. Tuned so
/// 48-piece A4-ish puzzles land at roughly the previous absolute
/// 1.5 mm clearance.
pub const KNOB_CLEARANCE_FRACTION: f64 = 0.06;

/// Lower bound on knob clearance (mm) regardless of how small the
/// puzzle's piece scale gets. Keeps knobs from sitting close enough
/// that the laser kerf merges the two cuts.
pub const MIN_KNOB_CLEARANCE_MM: f64 = 0.5;

/// Compute the actual clearance distance for a layout with the given
/// average knob `base`. Used by `resolve_knob_collisions` /
/// `knob_outer_boundary` to size their buffer shells proportionally
/// to the puzzle's piece scale.
pub fn knob_clearance_for_base(base: f64) -> f64 {
    (base * KNOB_CLEARANCE_FRACTION).max(MIN_KNOB_CLEARANCE_MM)
}

/// Post-Lloyd seed-rejection threshold: any non-anchor seed whose
/// clipped cell area falls below this fraction of the median cell
/// area gets dropped before the final Voronoi runs. Catches sliver
/// cells where the seed got squeezed between a whimsy contour and
/// the outer puzzle border — `merge_thin_pieces` can't always fix
/// those because they may have no shared edges to suppress, and
/// dropping the seed lets neighbors expand to absorb the region.
pub const MIN_CELL_AREA_FRACTION: f64 = 0.25;

/// Default number of Laplacian smoothing iterations run on the shared
/// edge mesh after CVT + merges. A few iterations noticeably flatten
/// the edge-length histogram without erasing the underlying Voronoi
/// structure; higher counts approach a regular hex grid.
pub const DEFAULT_SMOOTH_ITERATIONS: usize = 3;

/// Fraction of the average piece linear dimension used as the
/// "anchor protection radius" for Laplacian smoothing. Vertices
/// within this radius of any anchor seed are pinned so the bisector
/// through the sharp whimsy corner stays aligned.
pub const SMOOTH_PIN_RADIUS_FRACTION: f64 = 0.3;

/// Default edge length (mm) below which a connector is skipped entirely
/// and the edge renders as a straight line. Derived so that any edge at
/// or above this length can host a knob whose neck opening is at least
/// `MIN_KNOB_NECK_OPENING_MM`:
///
/// `neck_opening = NECK_OPENING_RATIO × min(edge, cross) = 0.125 × edge`,
/// so we need `edge ≥ 3 / 0.125 = 24 mm`.
pub const DEFAULT_MIN_KNOB_EDGE_LENGTH: f64 = MIN_KNOB_NECK_OPENING_MM / NECK_OPENING_RATIO;

/// Inputs for [`build_cvt_layout`].
pub struct CvtParams<'a> {
    /// Puzzle bounding-box width in mm.
    pub width: f64,
    /// Puzzle bounding-box height in mm.
    pub height: f64,
    /// Closed outer boundary. Cells are clipped to this region.
    pub boundary: &'a BezPath,
    /// Target number of pieces (equal to the number of Voronoi seeds).
    pub piece_count: usize,
    /// Deterministic seed string; drives both initial scatter and
    /// connector params.
    pub seed: &'a str,
    /// Lloyd relaxation iterations. [`DEFAULT_LLOYD_ITERATIONS`] is a
    /// good default.
    pub lloyd_iterations: usize,
    /// Flattening tolerance (mm) used when rasterising the boundary to
    /// a polygon for clipping. [`DEFAULT_FLATTEN_TOLERANCE`] is a good
    /// default.
    pub boundary_flatten_tolerance: f64,
    /// Edges shorter than this (mm) cause their two adjacent pieces to
    /// merge — handles thin tips and pinched necks where CVT would
    /// otherwise slice a narrow region into slivers.
    /// [`DEFAULT_MERGE_EDGE_THRESHOLD`] is a good default (0.0 disables).
    pub merge_edge_threshold: f64,
    /// Edges shorter than this (mm) render as a straight line; no knob.
    /// Keeps tiny cramped connectors from appearing where the Voronoi
    /// tessellation placed short faces.
    /// [`DEFAULT_MIN_KNOB_EDGE_LENGTH`] is a good default.
    pub min_knob_edge_length: f64,
    /// Pre-placed anchor seeds inserted *before* the random rejection
    /// scatter. Used to guarantee dedicated Voronoi cells near sharp
    /// whimsy corners (see [`find_sharp_corners`] +
    /// [`anchor_seeds_for_corner`]) so slivers don't form. Total seed
    /// count is `anchors.len() + piece_count` — anchors are additive.
    pub anchors: &'a [Point],
    /// Sliver-merge threshold (degrees). Pieces with any interior
    /// corner sharper than this AND below-median area get absorbed by
    /// their longest-edge neighbor. Set to `0.0` to skip the pass
    /// entirely — useful for nested CVTs where the caller asked for
    /// an exact piece count and wants every cell preserved even if
    /// the boundary produces acute corners (e.g. 3 seeds inside a
    /// heart, where one unavoidably touches the bottom tip).
    /// [`DEFAULT_MIN_PIECE_ANGLE_DEG`] is the default for top-level
    /// puzzles.
    pub min_piece_angle_deg: f64,
    /// Laplacian smoothing iterations applied to the shared edge
    /// mesh after CVT + merges. Evens out edge lengths, which in turn
    /// evens out knob sizes. Boundary-touching vertices and vertices
    /// near anchor seeds stay pinned. `0` disables.
    /// [`DEFAULT_SMOOTH_ITERATIONS`] is a good default.
    pub smooth_iterations: usize,
    /// Cell-generation algorithm. Selects between random-scatter+Lloyd
    /// (`Cvt`) and Bridson's Poisson disc sampling (`Poisson`). The
    /// downstream pipeline (Voronoi tessellation, edge extraction,
    /// merges, smoothing, connectors) is identical regardless of
    /// which algorithm produced the seeds.
    pub cell_algorithm: crate::config::CellAlgorithm,
}

/// Build a CVT-based [`PuzzleLayout`] from scattered seeds clipped to a
/// closed boundary shape.
///
/// Returns `Err(_)` if the initial rejection sampler can't place
/// `piece_count` seeds inside the boundary within a reasonable attempt
/// budget — typically a sign that the boundary is too small relative to
/// the requested piece count.
pub fn build_cvt_layout(params: &CvtParams) -> Result<PuzzleLayout, String> {
    if params.piece_count < 2 {
        return Err("CVT layout requires at least 2 pieces".to_string());
    }

    // Boundary polygon (used for winding-inside tests and linesweeper
    // clipping).
    let boundary_polygon = flatten_polygon(params.boundary, params.boundary_flatten_tolerance);
    if boundary_polygon.is_empty() {
        return Err("boundary is empty after flattening".to_string());
    }
    // Y-bucket spatial index: makes per-call polygon_contains O(N/B)
    // instead of O(N) where N=boundary segments, B=bucket count.
    // Lloyd's per-cell vertex check fires this thousands of times
    // per generation, so the one-time build cost (~ms at 200 segs)
    // pays back many times over.
    let boundary_index = BoundaryIndex::new(&boundary_polygon);

    // Bounding box for Voronoi — wrap the shape with some padding so
    // Voronoi cells extend past the shape, giving clean intersections.
    let (bb_min, bb_max) = polygon_bbox(&boundary_polygon);
    let pad = ((bb_max.x - bb_min.x).max(bb_max.y - bb_min.y)) * 0.1 + 1.0;
    let bbox_width = (bb_max.x - bb_min.x) + 2.0 * pad;
    let bbox_height = (bb_max.y - bb_min.y) + 2.0 * pad;
    let bbox_center = VPoint {
        x: (bb_min.x + bb_max.x) * 0.5,
        y: (bb_min.y + bb_max.y) * 0.5,
    };
    let voronoi_bbox = BoundingBox::new(bbox_center, bbox_width, bbox_height);

    // ─── Phase 1: Cell generation (algorithm-pluggable) ─────────
    //
    // Produces a `Vec<VPoint>` of seed positions inside the boundary.
    // Anchor seeds (sharp-corner positioning) ALWAYS come first so
    // downstream code can identify them by index range. Random/Lloyd
    // (CVT) and Bridson (Poisson) are the two algorithms today; new
    // algorithms slot in by adding a match arm here. Everything below
    // operates on the `seeds` Vec without caring how it was produced.
    let t = phase_now();
    let anchor_count = params.anchors.len();
    let initial_anchors: Vec<VPoint> = params
        .anchors
        .iter()
        .map(|p| VPoint { x: p.x, y: p.y })
        .collect();
    let mut seeds: Vec<VPoint> = match params.cell_algorithm {
        crate::config::CellAlgorithm::Classic => {
            return Err("Classic grid uses build_classic_layout, not Voronoi seeding".into());
        }
        crate::config::CellAlgorithm::Cvt => {
            // CVT: rejection-sample uniformly inside the boundary.
            // Lloyd relaxation runs below to drive these toward
            // centroidal positions.
            let mut seed_rng =
                create_rng(&format!("{}-positions", params.seed));
            scatter_seeds(
                &boundary_polygon,
                &boundary_index,
                bb_min,
                bb_max,
                anchor_count + params.piece_count,
                initial_anchors,
                &mut seed_rng,
            )?
        }
        crate::config::CellAlgorithm::Poisson => {
            // Poisson disc: Bridson's algorithm gives well-spaced
            // seeds in one pass, no relaxation needed. Cells will be
            // near-centroidal but not strictly so.
            let mut seed_rng =
                create_rng(&format!("{}-positions", params.seed));
            bridson_poisson_seeds(
                &boundary_polygon,
                &boundary_index,
                bb_min,
                bb_max,
                anchor_count + params.piece_count,
                initial_anchors,
                &mut seed_rng,
            )?
        }
    };
    phase_log("scatter_seeds", phase_now() - t);

    // ─── Phase 2: Lloyd relaxation (CVT only, skipped for Poisson) ─
    //
    // Each iteration rebuilds the Voronoi, clips cells to the
    // boundary, and moves seeds to clipped-cell centroids. Anchor
    // seeds (the first `anchors.len()` entries) stay pinned so the
    // bisector between each anchor pair keeps aligned with its sharp
    // corner through every iteration.
    //
    // Adaptive termination: track the maximum seed displacement each
    // iteration and bail out as soon as it falls below
    // `LLOYD_CONVERGENCE_DELTA_MM`. Determinism is preserved — Lloyd
    // converges to the same fixed point regardless of how many extra
    // (no-op) iterations we'd otherwise run.
    //
    // The `lloyd_iterations` cap is shared between algorithms: for
    // CVT it's the relaxation count from random scatter (typically
    // 20); for Poisson it's the *polish* count after Bridson, which
    // can be 0 (raw Poisson) up to a small handful (3–5 polishes
    // catch up to ~95% of full CVT quality). The caller decides;
    // [`build_cvt_layout`] just runs whatever it's told.
    let lloyd_cap = params.lloyd_iterations;
    let t = phase_now();
    let pinned = params.anchors.len();
    let mut iters_run = 0usize;
    for _ in 0..lloyd_cap {
        let voronoi = build_voronoi(&seeds, &voronoi_bbox)?;
        let prev_seeds = seeds.clone();
        seeds = relaxed_seeds(
            &voronoi,
            params.boundary,
            &boundary_polygon,
            &boundary_index,
            &seeds,
            pinned,
        );
        iters_run += 1;
        // Skip the convergence check on iteration 1 — seeds always
        // move significantly on the first relaxation pass and the
        // check is wasted work.
        if iters_run >= 2 {
            let mut max_delta_sq = 0.0_f64;
            for (a, b) in prev_seeds.iter().zip(seeds.iter()) {
                let dx = a.x - b.x;
                let dy = a.y - b.y;
                let d2 = dx * dx + dy * dy;
                if d2 > max_delta_sq {
                    max_delta_sq = d2;
                }
            }
            if max_delta_sq.sqrt() < LLOYD_CONVERGENCE_DELTA_MM {
                break;
            }
        }
    }
    phase_log("lloyd_loop", phase_now() - t);
    phase_log("  lloyd_iters_run", iters_run as f64);

    // Post-Lloyd seed filter: drop any non-anchor seed whose clipped
    // cell ended up too small (e.g. squeezed between a whimsy contour
    // and the outer puzzle border into a sliver pocket where its
    // bisectors with neighbors are entirely outside the boundary).
    // Anchors are preserved unconditionally — they exist precisely
    // because we want them at sharp corners. Neighbors expand to fill
    // any region a dropped seed previously occupied.
    //
    // Skipped if no Lloyd iterations ran (nothing's moved).
    let t = phase_now();
    if iters_run > 0 && seeds.len() > pinned + 2 {
        let scratch_voronoi = build_voronoi(&seeds, &voronoi_bbox)?;
        // Fast path: when all of a cell's Voronoi vertices lie inside
        // the boundary, the cell is fully contained and clipping is a
        // no-op — skip the expensive linesweeper intersection. Mirrors
        // the optimization in `relaxed_seeds`. Cuts O(n) linesweeper
        // calls down to O(boundary-ring) calls per generation.
        let scratch_vertices = scratch_voronoi.vertices();
        let scratch_areas: Vec<f64> = (0..seeds.len())
            .map(|i| {
                let cell_vertex_indices = &scratch_voronoi.cells()[i];
                let cell_fully_inside = cell_vertex_indices.iter().all(|&vi| {
                    let v = &scratch_vertices[vi];
                    polygon_contains_indexed(
                        &boundary_polygon,
                        &boundary_index,
                        Point::new(v.x, v.y),
                    )
                });
                if cell_fully_inside {
                    let cell = voronoi_cell_as_path(&scratch_voronoi, i);
                    polygon_area_simple(&cell)
                } else {
                    // Use the specialized convex clip here too —
                    // we just need the area for the median-area
                    // calculation, but a faster clip means a faster
                    // post-Lloyd filter pass.
                    let cell_pts =
                        voronoi_cell_vertices(&scratch_voronoi, i);
                    let clipped = crate::convex_clip::convex_clip(
                        &cell_pts,
                        &boundary_polygon,
                        &boundary_index,
                    );
                    polygon_area_simple(&clipped)
                }
            })
            .collect();
        // Median of clipped-cell areas as the reference scale.
        let mut sorted = scratch_areas.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];
        let area_floor = median * MIN_CELL_AREA_FRACTION;
        // Filter: keep all anchors plus every random seed whose cell
        // is at or above the floor.
        let kept: Vec<VPoint> = seeds
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < pinned || scratch_areas[*i] >= area_floor)
            .map(|(_, s)| s.clone())
            .collect();
        if kept.len() < seeds.len() && kept.len() >= 2 {
            seeds = kept;
        }
    }
    phase_log("post_lloyd_filter", phase_now() - t);

    // One final Voronoi + clip pass produces the piece polygons we
    // actually emit. Fully-interior cells skip clipping. Boundary-
    // touching cells use the specialized [`crate::convex_clip`]
    // routine — Greiner-Hörmann-style trace specialized for convex
    // subjects, ~10× faster per call than linesweeper. Property
    // tested against linesweeper across 100+ real-world cells; see
    // `convex_clip::tests::property_test_against_linesweeper`.
    let t = phase_now();
    let voronoi = build_voronoi(&seeds, &voronoi_bbox)?;
    let final_vertices = voronoi.vertices();
    let clipped_cells: Vec<BezPath> = (0..seeds.len())
        .map(|i| {
            let cell_vertex_indices = &voronoi.cells()[i];
            let cell_fully_inside = cell_vertex_indices.iter().all(|&vi| {
                let v = &final_vertices[vi];
                polygon_contains_indexed(
                    &boundary_polygon,
                    &boundary_index,
                    Point::new(v.x, v.y),
                )
            });
            if cell_fully_inside {
                voronoi_cell_as_path(&voronoi, i)
            } else {
                let cell_pts = voronoi_cell_vertices(&voronoi, i);
                crate::convex_clip::convex_clip(
                    &cell_pts,
                    &boundary_polygon,
                    &boundary_index,
                )
            }
        })
        .collect();
    phase_log("final_clip", phase_now() - t);

    // Build pieces (edge_indices populated after edges).
    let mut pieces: Vec<LayoutPiece> = seeds
        .iter()
        .enumerate()
        .map(|(i, s)| LayoutPiece {
            id: i,
            center: Point::new(s.x, s.y),
            edge_indices: Vec::new(),
            outline: Some(clipped_cells[i].clone()),
        })
        .collect();

    // Extract internal edges — one per unordered pair of neighbouring
    // cells whose shared Voronoi edge has a non-trivial inside-boundary
    // stretch.
    //
    // Knob direction is hashed from the (seed, sorted piece-pair) —
    // *not* drawn from a sequential RNG roll. This keeps each edge's
    // direction stable as Lloyd re-relaxes seeds during interactive
    // drags: the piece IDs (i, j) are stable across preview frames as
    // long as the same seeds are used, so the edge between pieces 12
    // and 47 always gets the same direction regardless of what order
    // the Voronoi reports its neighbors in. Without this, Voronoi
    // cell-iteration order shifts every iteration and knob directions
    // flicker visibly during drag.
    let t = phase_now();
    let direction_seed = crate::seed::hash_seed(&format!("{}-directions", params.seed));
    let mut edges: Vec<LayoutEdge> = Vec::new();
    for i in 0..seeds.len() {
        let cell = voronoi.cell(i);
        for j in cell.iter_neighbors() {
            if j <= i {
                // Emit each unordered pair once.
                continue;
            }
            let Some((a, b)) = shared_voronoi_edge(&voronoi, i, j) else {
                continue;
            };
            // A Voronoi edge crossing a concavity or hole yields
            // multiple inside-boundary sub-segments — each is a real
            // shared-edge piece between cells i and j.
            let direction = if crate::seed::hash_pair_bit(direction_seed, i, j) {
                TabDirection::Out
            } else {
                TabDirection::In
            };
            for (clip_a, clip_b) in
                clip_segment_to_polygon(a, b, &boundary_polygon, &boundary_index)
            {
                if (clip_b - clip_a).hypot() < 1e-3 {
                    continue;
                }
                let edge_idx = edges.len();
                edges.push(LayoutEdge {
                    start: clip_a,
                    end: clip_b,
                    direction,
                    connector: None,
                    connector_params: None,
                    pieces: (i, j),
                });
                pieces[i].edge_indices.push(edge_idx);
                pieces[j].edge_indices.push(edge_idx);
            }
        }
    }

    let mut layout = PuzzleLayout {
        width: params.width,
        height: params.height,
        outer_boundary: params.boundary.clone(),
        edges,
        pieces,
        knob_base_cap: 0.0,
    };
    phase_log("edge_extract", phase_now() - t);

    // Sliver-merge first — a sharp whimsy/border tip (heart bottom,
    // star point) can produce a CVT cell shaped like a toothpick that
    // would break in real material. Merging it into a neighbor folds
    // the tip into a larger, structurally-sound piece. Skipped when
    // `min_piece_angle_deg == 0.0` (nested CVTs preserve the exact
    // piece count the caller requested).
    let t = phase_now();
    if params.min_piece_angle_deg > 0.0 {
        layout.merge_thin_pieces(params.min_piece_angle_deg);
    }
    phase_log("merge_thin_pieces", phase_now() - t);

    // Then merge any remaining piece pairs whose shared edge is too
    // short to host a useful knob. A chain of short edges (e.g. multiple
    // seeds in a thin strip) collapses into one piece in a single call.
    let t = phase_now();
    layout.merge_short_edges(params.merge_edge_threshold);
    phase_log("merge_short_edges", phase_now() - t);

    // Laplacian smoothing of the shared edge mesh — evens out edge
    // lengths so knob sizes (which scale with edge length) are more
    // uniform. Boundary-touching vertices are already pinned inside
    // `smooth_edges` via the degree-<3 rule; vertices within
    // `SMOOTH_PIN_RADIUS_FRACTION × avg_piece_dim` of an anchor seed
    // also stay pinned so the sharp-corner bisector geometry doesn't
    // drift.
    let avg_piece_dim = ((params.width * params.height)
        / (params.piece_count as f64).max(1.0))
        .sqrt();
    let pin_radius = avg_piece_dim * SMOOTH_PIN_RADIUS_FRACTION;
    let t = phase_now();
    layout.smooth_edges(params.smooth_iterations, params.anchors, pin_radius);
    phase_log("smooth_edges", phase_now() - t);

    let t = phase_now();
    layout.generate_connectors(
        &ClassicKnobConnector,
        params.seed,
        params.min_knob_edge_length,
    );
    phase_log("generate_connectors", phase_now() - t);

    // NOTE: `compute_and_cap_avg_knob_base` and `resolve_knob_collisions`
    // are NOT called here — callers run them explicitly via
    // `finalize_cvt_layout`.

    Ok(layout)
}

/// Convenience wrapper that runs the full default post-CVT pipeline:
/// caps knob bases, then resolves knob collisions with the standard
/// proportional clearance.
pub fn finalize_cvt_layout(layout: &mut PuzzleLayout) {
    layout.compute_and_cap_avg_knob_base(&ClassicKnobConnector);
    let clearance = knob_clearance_for_base(layout.knob_base_cap);
    layout.resolve_knob_collisions(&ClassicKnobConnector, clearance, &[]);
}

// ─── Boundary flattening ─────────────────────────────────────────────
//
// `flatten_polygon` lives in `crate::flat_boundary` so it can be
// shared with `polygon_contains` (which needs the same per-subpath
// representation for fast point-in-region tests).

fn polygon_bbox(subpaths: &[Vec<Point>]) -> (Point, Point) {
    let mut min = Point::new(f64::INFINITY, f64::INFINITY);
    let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
    for sub in subpaths {
        for p in sub {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
    }
    (min, max)
}

// ─── Sharp-corner detection (for anchor seed placement) ─────────────

/// A sharp convex corner on a closed-path boundary, in world
/// coordinates. `axis` is a unit vector pointing *outward* from the
/// shape interior (i.e. into the puzzle region, for a whimsy hole).
#[derive(Debug, Clone, Copy)]
pub struct SharpCorner {
    pub position: Point,
    pub axis: Vec2,
}

/// Walk `path` and return every convex corner (from the path's own
/// orientation) whose interior angle is ≤ `max_angle_deg`. The `axis`
/// on each returned corner points opposite the angle bisector — away
/// from the enclosed interior of the path.
///
/// Flattening tolerance matches the rest of the CVT pipeline (0.25 mm).
/// Very short neighbor segments (numerical noise near a cusp) are
/// stepped past so the measured angle reflects the real corner tangents
/// rather than flattening artifacts.
pub fn find_sharp_corners(path: &BezPath, max_angle_deg: f64) -> Vec<SharpCorner> {
    // Minimum distance to consider a neighbor as a directional sample
    // (mm). Smaller than a CVT bisector edge, larger than typical
    // flattening noise at a sharp cusp.
    const MIN_NEIGHBOR_MM: f64 = 0.5;

    let threshold = max_angle_deg.to_radians();
    let mut out: Vec<SharpCorner> = Vec::new();

    // Flatten into one Vec<Point> per subpath, deduping trailing
    // close-path duplicates so a cusp that happens to land at the
    // subpath's start/end doesn't get hidden behind a zero-length
    // neighbor.
    let mut subpaths: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    kurbo::flatten(path.iter(), DEFAULT_FLATTEN_TOLERANCE, |el| match el {
        PathEl::MoveTo(p) => {
            if !current.is_empty() {
                if current.len() >= 2
                    && (current[current.len() - 1] - current[0]).hypot() < 1e-6
                {
                    current.pop();
                }
                subpaths.push(std::mem::take(&mut current));
            }
            current.push(p);
        }
        PathEl::LineTo(p) => current.push(p),
        _ => {}
    });
    if !current.is_empty() {
        if current.len() >= 2
            && (current[current.len() - 1] - current[0]).hypot() < 1e-6
        {
            current.pop();
        }
        subpaths.push(current);
    }

    for sub in &subpaths {
        let n = sub.len();
        if n < 3 {
            continue;
        }

        // Polygon winding direction via shoelace sign. In screen
        // coordinates (y-down) this tells us which cross-product sign
        // at a vertex means "convex" (protruding away from the interior).
        let mut shoelace = 0.0;
        for i in 0..n {
            let j = (i + 1) % n;
            shoelace += sub[i].x * sub[j].y - sub[j].x * sub[i].y;
        }
        if shoelace.abs() < 1e-9 {
            continue; // degenerate
        }
        let winding_sign = shoelace.signum();

        for i in 0..n {
            let curr = sub[i];
            // Step backward/forward until we find vertices at least
            // MIN_NEIGHBOR_MM away — skips flattening noise so the
            // tangent sample is reliable.
            let prev = nearest_neighbor(sub, i, -1, MIN_NEIGHBOR_MM);
            let next = nearest_neighbor(sub, i, 1, MIN_NEIGHBOR_MM);
            let (Some(prev), Some(next)) = (prev, next) else {
                continue;
            };
            let u = prev - curr;
            let v = next - curr;
            let lu = u.hypot();
            let lv = v.hypot();
            if lu < 1e-9 || lv < 1e-9 {
                continue;
            }

            // Convexity: sign of the cross of (curr-prev) × (next-curr).
            // On a CCW-wound polygon (shoelace > 0 in screen coords...
            // actually depends; the sign check against winding handles
            // either convention).
            let d_in = Vec2::new(curr.x - prev.x, curr.y - prev.y);
            let d_out = Vec2::new(next.x - curr.x, next.y - curr.y);
            let cross = d_in.x * d_out.y - d_in.y * d_out.x;
            if cross.signum() != winding_sign {
                // Concave vertex — no sliver risk on the puzzle side.
                continue;
            }

            let cos = ((u.x * v.x + u.y * v.y) / (lu * lv)).clamp(-1.0, 1.0);
            let angle = cos.acos();
            if angle > threshold {
                continue;
            }

            // Bisector of the two outgoing vectors (u and v), unit
            // length. This points INTO the polygon interior at a
            // convex vertex. Flip it to get the outward axis (into
            // the puzzle region for a whimsy hole).
            let u_hat = Vec2::new(u.x / lu, u.y / lu);
            let v_hat = Vec2::new(v.x / lv, v.y / lv);
            let bisector = u_hat + v_hat;
            let bl = bisector.hypot();
            if bl < 1e-6 {
                // u and v are nearly anti-parallel (a 180° vertex,
                // basically a straight line) — should have been caught
                // by the angle threshold, but guard anyway.
                continue;
            }
            let axis = Vec2::new(-bisector.x / bl, -bisector.y / bl);
            out.push(SharpCorner {
                position: curr,
                axis,
            });
        }
    }
    out
}

/// Step forward (`dir = 1`) or backward (`dir = -1`) from `i` around
/// the closed polygon `sub` until we find a vertex at least
/// `min_dist_mm` away from `sub[i]`. Returns `None` if no such vertex
/// exists (a degenerate subpath made entirely of near-coincident points).
fn nearest_neighbor(sub: &[Point], i: usize, dir: i32, min_dist_mm: f64) -> Option<Point> {
    let n = sub.len();
    let start = sub[i];
    for step in 1..n {
        let idx = if dir > 0 {
            (i + step) % n
        } else {
            (i + n - step) % n
        };
        let p = sub[idx];
        if (p - start).hypot() >= min_dist_mm {
            return Some(p);
        }
    }
    None
}

/// Place a pair of anchor seeds equidistant from `corner.position` so
/// their perpendicular bisector passes through the corner. The
/// preferred placement uses `corner.axis` (the corner's outward
/// direction) so the pair sits symmetric across that axis; if either
/// seed would land outside `boundary` (e.g. another whimsy occupies
/// that region), the axis is rotated around the corner in a search
/// sweep until both seeds fit. Returns `None` only if no rotation
/// places both seeds inside.
///
/// Geometrically: given corner position `C`, axis direction `n`
/// (unit), tangent `t = perpendicular(n)`, axial offset `d_n`, and
/// tangential spread `d_t`, the two seeds are:
///
/// - `seed_a = C + n*d_n - t*d_t`
/// - `seed_b = C + n*d_n + t*d_t`
///
/// Both seeds are at distance `sqrt(d_n² + d_t²)` from `C`, so they
/// lie on a circle around `C` and `C` is on the perpendicular
/// bisector of the segment `seed_a seed_b`. Rotating `n` rotates that
/// bisector around `C` but the corner remains on the bisector — the
/// "split the corner down the middle" property is preserved at every
/// rotation.
pub fn anchor_seeds_for_corner(
    corner: &SharpCorner,
    boundary: &BezPath,
    avg_piece_dim: f64,
) -> Option<(Point, Point)> {
    let offset = avg_piece_dim * ANCHOR_OFFSET_FRACTION;
    let spread = avg_piece_dim * ANCHOR_SPREAD_FRACTION;

    // Search rotations around the corner: the outward axis first
    // (preserves the natural symmetric placement), then progressively
    // wider deflections to either side. Stops at ±75° because beyond
    // that the axis points back along the boundary tangent and the
    // anchors would be approximately on the boundary itself.
    const ROTATIONS_DEG: [f64; 11] = [
        0.0, 15.0, -15.0, 30.0, -30.0, 45.0, -45.0, 60.0, -60.0, 75.0, -75.0,
    ];

    for &rot_deg in &ROTATIONS_DEG {
        let (s, c) = rot_deg.to_radians().sin_cos();
        let n = Vec2::new(
            corner.axis.x * c - corner.axis.y * s,
            corner.axis.x * s + corner.axis.y * c,
        );
        let t = Vec2::new(-n.y, n.x);
        let midpoint = Point::new(
            corner.position.x + n.x * offset,
            corner.position.y + n.y * offset,
        );
        let a = Point::new(midpoint.x - t.x * spread, midpoint.y - t.y * spread);
        let b = Point::new(midpoint.x + t.x * spread, midpoint.y + t.y * spread);
        if boundary.winding(a) != 0 && boundary.winding(b) != 0 {
            return Some((a, b));
        }
    }
    None
}

// ─── Seed scatter ────────────────────────────────────────────────────

/// Uniform rejection sampling inside the boundary: pick random points in
/// the shape's bounding rectangle, keep the ones with nonzero winding.
/// Errors if `count` seeds can't be placed within the attempt budget —
/// a sign that the boundary is tiny relative to the request.
///
/// `initial` seeds (e.g. geometry-derived anchors near sharp corners)
/// are placed first, then the rejection sampler tops the vector up to
/// `count`. Initial seeds are assumed to already lie inside the
/// boundary — callers are responsible for filtering them if not.
fn scatter_seeds(
    boundary_polygon: &[Vec<Point>],
    boundary_index: &BoundaryIndex,
    bb_min: Point,
    bb_max: Point,
    count: usize,
    initial: Vec<VPoint>,
    rng: &mut rand_chacha::ChaCha8Rng,
) -> Result<Vec<VPoint>, String> {
    let mut seeds = initial;
    seeds.reserve(count.saturating_sub(seeds.len()));
    // Budget: enough for reasonable rejection rates (per-seed ~100-500
    // tries for a shape filling ~half its bbox), capped so a pathological
    // tiny boundary can't burn minutes before we give up.
    let max_attempts = count.saturating_mul(500).clamp(5_000, 200_000);
    let mut attempts = 0;
    while seeds.len() < count && attempts < max_attempts {
        let x: f64 = rng.random::<f64>() * (bb_max.x - bb_min.x) + bb_min.x;
        let y: f64 = rng.random::<f64>() * (bb_max.y - bb_min.y) + bb_min.y;
        let p = Point::new(x, y);
        if polygon_contains_indexed(boundary_polygon, boundary_index, p) {
            seeds.push(VPoint { x, y });
        }
        attempts += 1;
    }
    if seeds.len() < count {
        return Err(format!(
            "rejection sampler placed {}/{} seeds — boundary may be too small",
            seeds.len(),
            count
        ));
    }
    Ok(seeds)
}

/// Bridson's Poisson disc sampling. Places seeds inside the boundary
/// such that no two seeds are closer than a target minimum distance,
/// without an iterative relaxation pass.
///
/// Algorithm (R. Bridson, 2007, "Fast Poisson Disk Sampling in
/// Arbitrary Dimensions"):
///
/// 1. Compute target minimum distance `r` from the desired seed count
///    and boundary area.
/// 2. Maintain a background grid with cell size `r/√2` so each cell
///    holds at most one sample. Distance checks become O(1).
/// 3. Seed the active list (the `initial` anchors, or one random
///    boundary-interior point if anchors are empty).
/// 4. While the active list is non-empty: pick a random active point,
///    try `MAX_ATTEMPTS` random candidate placements in the annulus
///    [r, 2r] around it. First valid candidate (inside the boundary
///    and ≥ r from every existing sample) is added. If all attempts
///    fail, retire the active point.
/// 5. Stop when target count reached or active list empty.
///
/// The result is "blue noise": well-spaced, roughly hexagonally
/// packed, perceptually random. Voronoi cells over these points are
/// already close to centroidal — typically ~85% of full Lloyd-relaxed
/// CVT quality with no iteration.
///
/// Determinism: same seed string + same boundary + same anchors →
/// same RNG sequence → same point placement order → same output.
///
/// Output count is approximately `target_count` but may vary by ±10%
/// due to packing effects; downstream merge passes absorb the
/// variation in the same way they do for CVT's post-Lloyd filter.
fn bridson_poisson_seeds(
    boundary_polygon: &[Vec<Point>],
    boundary_index: &BoundaryIndex,
    bb_min: Point,
    bb_max: Point,
    target_count: usize,
    initial: Vec<VPoint>,
    rng: &mut rand_chacha::ChaCha8Rng,
) -> Result<Vec<VPoint>, String> {
    /// Bridson's stock "30 attempts per active point" budget.
    const MAX_ATTEMPTS: usize = 30;
    /// Lower bound of the placement annulus, as a multiple of `r`.
    /// Equals `r` itself — placements at this distance are exactly the
    /// minimum allowed.
    const ANNULUS_LO: f64 = 1.0;
    /// Upper bound of the placement annulus, as a multiple of `r`.
    /// Bridson's stock value is `2.0`. We use a tighter `1.3` so new
    /// points cluster closer to their parent, which substantially
    /// reduces cell-size variance in the resulting Voronoi diagram —
    /// the typical gripe with vanilla Bridson is "biggest cell ~3×
    /// smallest"; with `1.3r` it's closer to `1.5×`. Determinism is
    /// preserved (still RNG-driven), just with a tighter distribution.
    const ANNULUS_HI: f64 = 1.3;

    let bbox_w = bb_max.x - bb_min.x;
    let bbox_h = bb_max.y - bb_min.y;
    let bbox_area = bbox_w * bbox_h;

    // Target minimum distance. With Bridson's actual packing density
    // — `~0.65 A / r²` for the stock `[r, 2r]` annulus, but `~0.85 A
    // / r²` for the tighter `[r, 1.3r]` annulus — the relationship
    // between sample count N and min distance r in area A is roughly:
    //
    //     N ≈ 0.85 × A / r²    →    r ≈ √(0.85 × A / N)
    //
    // We undershoot slightly (factor 0.75) to bias toward producing
    // *at least* `target_count` seeds rather than fewer; rejecting
    // is easier than re-running with a smaller r.
    let r = (0.75 * bbox_area / (target_count as f64).max(1.0)).sqrt();
    if r <= 0.0 {
        return Err("Poisson: degenerate target distance".to_string());
    }
    let r2 = r * r;

    // Background grid with cell size `r/√2`. Diagonal of one cell
    // equals `r`, so two points in the same cell would be ≤ r apart
    // — the Bridson invariant means at most one sample per cell.
    let cell_size = r / std::f64::consts::SQRT_2;
    let grid_w = ((bbox_w / cell_size).ceil() as usize).max(1);
    let grid_h = ((bbox_h / cell_size).ceil() as usize).max(1);
    // Cell stores `Vec<usize>` (sample indices) rather than a single
    // `Option<usize>`: anchors might collide in the same cell since
    // they bypass the min-distance invariant, and we'd rather keep
    // both than silently drop one.
    let mut grid: Vec<Vec<usize>> = vec![Vec::new(); grid_w * grid_h];
    let cell_idx = |p: Point| -> (usize, usize) {
        let cx = ((p.x - bb_min.x) / cell_size).floor() as isize;
        let cy = ((p.y - bb_min.y) / cell_size).floor() as isize;
        (
            cx.clamp(0, grid_w as isize - 1) as usize,
            cy.clamp(0, grid_h as isize - 1) as usize,
        )
    };

    let mut samples: Vec<VPoint> = Vec::with_capacity(target_count);
    let mut active: Vec<usize> = Vec::new();

    // Pre-place anchors: they're forced positions tied to whimsy
    // sharp corners and must survive into the output. They go into
    // the active list so Poisson placement naturally densifies
    // around them.
    for vp in initial {
        let p = Point::new(vp.x, vp.y);
        let (cx, cy) = cell_idx(p);
        grid[cy * grid_w + cx].push(samples.len());
        active.push(samples.len());
        samples.push(vp);
    }

    // If no anchors, find a random valid starting point inside the
    // boundary. Same rejection-sample loop as `scatter_seeds`.
    if samples.is_empty() {
        let mut found: Option<Point> = None;
        for _ in 0..1000 {
            let x: f64 = rng.random::<f64>() * bbox_w + bb_min.x;
            let y: f64 = rng.random::<f64>() * bbox_h + bb_min.y;
            let p = Point::new(x, y);
            if polygon_contains_indexed(boundary_polygon, boundary_index, p) {
                found = Some(p);
                break;
            }
        }
        let initial_p = found.ok_or_else(|| {
            "Poisson: couldn't find an interior starting point".to_string()
        })?;
        let (cx, cy) = cell_idx(initial_p);
        grid[cy * grid_w + cx].push(samples.len());
        active.push(samples.len());
        samples.push(VPoint {
            x: initial_p.x,
            y: initial_p.y,
        });
    }

    // Main loop. Active list shrinks as we retire fully-explored
    // active points, grows as we place new ones. Terminates when
    // active is empty or we've reached the target.
    while !active.is_empty() && samples.len() < target_count {
        // Random selection from active list. Bridson's classic
        // formulation. swap_remove is O(1) for retiring a point.
        let active_pos = (rng.random::<f64>() * active.len() as f64) as usize;
        let active_pos = active_pos.min(active.len() - 1);
        let center_idx = active[active_pos];
        let center = samples[center_idx].clone();

        let mut placed = false;
        for _ in 0..MAX_ATTEMPTS {
            // Random angle, random radius in [ANNULUS_LO·r,
            // ANNULUS_HI·r). The annulus (not a disc) avoids placing
            // too close to the parent and avoids placing too far
            // where the existing point wouldn't have triggered
            // placement anyway.
            let angle = rng.random::<f64>() * std::f64::consts::TAU;
            let radius =
                r * (ANNULUS_LO + (ANNULUS_HI - ANNULUS_LO) * rng.random::<f64>());
            let candidate = Point::new(
                center.x + angle.cos() * radius,
                center.y + angle.sin() * radius,
            );

            // Must be inside the boundary (including: outside any
            // whimsy holes, since hole subpaths flip even-odd parity).
            if !polygon_contains_indexed(
                boundary_polygon,
                boundary_index,
                candidate,
            ) {
                continue;
            }

            // Distance check via the background grid. With cell size
            // r/√2, a sample within distance r could be in the same
            // cell or one of the surrounding ±2 cells.
            let (cx, cy) = cell_idx(candidate);
            let mut too_close = false;
            'outer: for dy in -2_isize..=2 {
                let ny = cy as isize + dy;
                if ny < 0 || ny >= grid_h as isize {
                    continue;
                }
                for dx in -2_isize..=2 {
                    let nx = cx as isize + dx;
                    if nx < 0 || nx >= grid_w as isize {
                        continue;
                    }
                    for &s_idx in &grid[ny as usize * grid_w + nx as usize] {
                        let other = &samples[s_idx];
                        let ddx = other.x - candidate.x;
                        let ddy = other.y - candidate.y;
                        if ddx * ddx + ddy * ddy < r2 {
                            too_close = true;
                            break 'outer;
                        }
                    }
                }
            }
            if too_close {
                continue;
            }

            // Place the sample.
            grid[cy * grid_w + cx].push(samples.len());
            active.push(samples.len());
            samples.push(VPoint {
                x: candidate.x,
                y: candidate.y,
            });
            placed = true;
            break;
        }

        if !placed {
            // All attempts failed — this active point can't grow any
            // further. Retire it via swap_remove (O(1)).
            active.swap_remove(active_pos);
        }
    }

    if samples.len() < 2 {
        return Err(format!(
            "Poisson disc placed {}/{} seeds — boundary may be too small",
            samples.len(),
            target_count
        ));
    }
    Ok(samples)
}

// ─── Voronoi via voronoice ───────────────────────────────────────────

fn build_voronoi(seeds: &[VPoint], bbox: &BoundingBox) -> Result<Voronoi, String> {
    VoronoiBuilder::default()
        .set_sites(seeds.to_vec())
        .set_bounding_box(bbox.clone())
        .set_clip_behavior(ClipBehavior::Clip)
        .build()
        .ok_or_else(|| "voronoice failed to build diagram".to_string())
}

/// Extract a Voronoi cell's vertices as a closed `BezPath`.
fn voronoi_cell_as_path(voronoi: &Voronoi, site: usize) -> BezPath {
    let mut path = BezPath::new();
    let mut first: Option<Point> = None;
    for (idx, v) in voronoi.cell(site).iter_vertices().enumerate() {
        let p = Point::new(v.x, v.y);
        if idx == 0 {
            path.move_to(p);
            first = Some(p);
        } else {
            path.line_to(p);
        }
    }
    if first.is_some() {
        path.close_path();
    }
    path
}

/// Extract a Voronoi cell's vertices as a `Vec<Point>` in the order
/// `voronoice` reports them. Used as input to `convex_clip` (which
/// takes a slice of points rather than a BezPath).
fn voronoi_cell_vertices(voronoi: &Voronoi, site: usize) -> Vec<Point> {
    voronoi
        .cell(site)
        .iter_vertices()
        .map(|v| Point::new(v.x, v.y))
        .collect()
}

// ─── Clipping ────────────────────────────────────────────────────────

/// Clip a cell polygon (closed) to the boundary. Returns the result as
/// a single `BezPath` (may contain multiple sub-paths if the cell is
/// split by a non-convex boundary).
fn clip_path_to_boundary(cell: &BezPath, boundary: &BezPath) -> BezPath {
    match mask_intersection(cell, boundary) {
        Ok(clipped) => clipped,
        Err(_) => BezPath::new(),
    }
}

/// Clip a straight line segment against the flattened boundary. Returns
/// **every** inside-boundary sub-segment, in order along `a → b`, not
/// just the longest — a segment that crosses a concave part of the
/// shape (e.g. the top dip of a heart, or a whimsy hole) has two or
/// more valid inside intervals that are all real shared-edge pieces.
///
/// Intersections are taken against each subpath's flattened line
/// segments. Midpoint classification uses [`polygon_contains`] on the
/// same flattened polygon — semantically equivalent to the original
/// [`BezPath::winding`] within the flatten tolerance, but with no
/// curve evaluation per call (this is invoked O(m) times).
fn clip_segment_to_polygon(
    a: Point,
    b: Point,
    subpaths: &[Vec<Point>],
    boundary_index: &BoundaryIndex,
) -> Vec<(Point, Point)> {
    use kurbo::{Line, PathSeg};
    let seg = PathSeg::Line(Line::new(a, b));

    let mut ts: Vec<f64> = vec![0.0, 1.0];
    for sub in subpaths {
        let n = sub.len();
        if n < 2 {
            continue;
        }
        for k in 0..n {
            let p0 = sub[k];
            let p1 = sub[(k + 1) % n];
            let bl = Line::new(p0, p1);
            for inter in seg.intersect_line(bl) {
                let t = inter.segment_t.clamp(0.0, 1.0);
                ts.push(t);
            }
        }
    }
    ts.sort_by(|x, y| x.partial_cmp(y).unwrap());

    let lerp = |t: f64| -> Point {
        Point::new(a.x + t * (b.x - a.x), a.y + t * (b.y - a.y))
    };

    let mut intervals: Vec<(Point, Point)> = Vec::new();
    for w in ts.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        if t1 - t0 < 1e-9 {
            continue;
        }
        let mid = lerp((t0 + t1) * 0.5);
        if !polygon_contains_indexed(subpaths, boundary_index, mid) {
            continue;
        }
        intervals.push((lerp(t0), lerp(t1)));
    }
    intervals
}

// ─── Lloyd relaxation step ───────────────────────────────────────────

/// Produce relaxed seed positions: centroid of each clipped cell, or
/// the original seed if the cell clipped to nothing.
///
/// Interior-cell fast path: when *all* of a cell's Voronoi vertices
/// lie inside the boundary, the cell is already fully contained and
/// clipping would return the same polygon anyway — we skip the
/// expensive `linesweeper` intersection and compute the centroid
/// directly on the Voronoi polygon. For rectangular puzzles this
/// applies to every non-border cell; for heart/star it still catches
/// ~half the cells each iteration.
fn relaxed_seeds(
    voronoi: &Voronoi,
    boundary: &BezPath,
    boundary_polygon: &[Vec<Point>],
    boundary_index: &BoundaryIndex,
    current: &[VPoint],
    pinned_prefix: usize,
) -> Vec<VPoint> {
    (0..current.len())
        .map(|i| {
            // Anchor seeds (pre-placed symmetric pairs near sharp
            // whimsy corners) stay pinned for every Lloyd iteration —
            // Lloyd equilibrates the *random* seeds around them. This
            // guarantees the bisector between each anchor pair passes
            // through its corner at the end of relaxation, not just
            // at the start.
            if i < pinned_prefix {
                return current[i].clone();
            }
            let cell_vertex_indices = &voronoi.cells()[i];
            let vertices = voronoi.vertices();
            let cell_fully_inside = cell_vertex_indices.iter().all(|&vi| {
                let v = &vertices[vi];
                polygon_contains_indexed(
                    boundary_polygon,
                    boundary_index,
                    Point::new(v.x, v.y),
                )
            });
            let centroid = if cell_fully_inside {
                let cell = voronoi_cell_as_path(voronoi, i);
                polygon_centroid(&cell)
            } else {
                // Lloyd's hot path. `convex_clip` replaces
                // linesweeper here — at ~150 boundary cells per
                // iteration × 14 iterations, this is the dominant
                // remaining clipping cost in CVT generation.
                let cell_pts = voronoi_cell_vertices(voronoi, i);
                let clipped = crate::convex_clip::convex_clip(
                    &cell_pts,
                    boundary_polygon,
                    boundary_index,
                );
                polygon_centroid(&clipped)
            };
            // Suppress unused-arg warning when boundary BezPath is no
            // longer needed for clipping but kept in the signature
            // for any future fallback path.
            let _ = boundary;
            match centroid {
                Some(c) => VPoint { x: c.x, y: c.y },
                None => current[i].clone(),
            }
        })
        .collect()
}

/// Area-weighted centroid of a (possibly multi-subpath) flat polygon
/// BezPath. Uses the shoelace formula per subpath and sums. Returns
/// `None` if the polygon has zero area.
/// Lightweight area calculation for a clipped CVT cell — flatten,
/// shoelace, abs/2. Returns `0.0` for degenerate or empty paths.
/// Used by the post-Lloyd seed filter where we just need a scale-
/// comparison value, not full polygon analysis.
fn polygon_area_simple(path: &BezPath) -> f64 {
    let mut subpaths: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    kurbo::flatten(path.iter(), 0.5, |el| match el {
        PathEl::MoveTo(p) => {
            if !current.is_empty() {
                subpaths.push(std::mem::take(&mut current));
            }
            current.push(p);
        }
        PathEl::LineTo(p) => current.push(p),
        _ => {}
    });
    if !current.is_empty() {
        subpaths.push(current);
    }
    let mut signed = 0.0;
    for sub in &subpaths {
        let n = sub.len();
        if n < 3 {
            continue;
        }
        for i in 0..n {
            let j = (i + 1) % n;
            signed += sub[i].x * sub[j].y - sub[j].x * sub[i].y;
        }
    }
    signed.abs() * 0.5
}

fn polygon_centroid(path: &BezPath) -> Option<Point> {
    let mut total_area = 0.0;
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut subpath_start: Option<Point> = None;
    let mut poly: Vec<Point> = Vec::new();

    let flush = |poly: &mut Vec<Point>,
                 total_area: &mut f64,
                 cx: &mut f64,
                 cy: &mut f64| {
        if poly.len() < 3 {
            poly.clear();
            return;
        }
        let mut area = 0.0;
        let mut sx = 0.0;
        let mut sy = 0.0;
        for k in 0..poly.len() {
            let p0 = poly[k];
            let p1 = poly[(k + 1) % poly.len()];
            let cross = p0.x * p1.y - p1.x * p0.y;
            area += cross;
            sx += (p0.x + p1.x) * cross;
            sy += (p0.y + p1.y) * cross;
        }
        area *= 0.5;
        if area.abs() > 1e-12 {
            *total_area += area;
            *cx += sx / 6.0;
            *cy += sy / 6.0;
        }
        poly.clear();
    };

    for el in path.iter() {
        match el {
            PathEl::MoveTo(p) => {
                flush(&mut poly, &mut total_area, &mut cx, &mut cy);
                subpath_start = Some(p);
                poly.push(p);
            }
            PathEl::LineTo(p) => {
                poly.push(p);
            }
            PathEl::ClosePath => {
                flush(&mut poly, &mut total_area, &mut cx, &mut cy);
                let _ = subpath_start;
            }
            _ => {
                // Flattened polygons shouldn't contain curves, but if
                // they do, treat the endpoint as a line step.
                if let PathEl::CurveTo(_, _, p) | PathEl::QuadTo(_, p) = el {
                    poly.push(p);
                }
            }
        }
    }
    flush(&mut poly, &mut total_area, &mut cx, &mut cy);

    if total_area.abs() < 1e-12 {
        return None;
    }
    Some(Point::new(cx / total_area, cy / total_area))
}

// ─── Shared Voronoi edge extraction ──────────────────────────────────

/// Return the straight-line Voronoi edge shared between cells `i` and
/// `j` — the two vertices that appear consecutively in both cells'
/// vertex lists (with opposite orientations, since the two cells walk
/// their boundaries in opposite directions around the shared edge).
/// Returns `None` if the cells don't share an edge in the Voronoi
/// diagram.
fn shared_voronoi_edge(voronoi: &Voronoi, i: usize, j: usize) -> Option<(Point, Point)> {
    let cells = voronoi.cells();
    let verts = voronoi.vertices();
    let ci: &Vec<usize> = &cells[i];
    let cj: &Vec<usize> = &cells[j];
    if ci.len() < 2 || cj.len() < 2 {
        return None;
    }
    // For each consecutive pair in cell i's vertex list, look for the
    // reverse pair in cell j's list.
    for k in 0..ci.len() {
        let a = ci[k];
        let b = ci[(k + 1) % ci.len()];
        for m in 0..cj.len() {
            let aj = cj[m];
            let bj = cj[(m + 1) % cj.len()];
            if a == bj && b == aj {
                let va = &verts[a];
                let vb = &verts[b];
                return Some((Point::new(va.x, va.y), Point::new(vb.x, vb.y)));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::{heart_path, star_path};

    fn rect_path(x: f64, y: f64, w: f64, h: f64) -> BezPath {
        let mut path = BezPath::new();
        path.move_to(Point::new(x, y));
        path.line_to(Point::new(x + w, y));
        path.line_to(Point::new(x + w, y + h));
        path.line_to(Point::new(x, y + h));
        path.close_path();
        path
    }

    #[test]
    fn test_find_sharp_corners_heart_tip() {
        // Heart has one very sharp convex corner at its bottom tip.
        let path = heart_path(200.0, 150.0);
        let corners = find_sharp_corners(&path, DEFAULT_SHARP_CORNER_ANGLE_DEG);
        assert_eq!(corners.len(), 1, "heart should yield exactly one sharp corner (bottom tip)");
        // Bottom tip sits at (width/2, height) = (100, 150). Axis
        // should point OUT of the heart (into the puzzle region
        // beyond the tip).
        let c = corners[0];
        assert!((c.position.x - 100.0).abs() < 1.0, "tip x ≈ 100: {:?}", c.position);
        assert!((c.position.y - 150.0).abs() < 1.0, "tip y ≈ 150: {:?}", c.position);
        // Outward axis for a heart tip points DOWN (y-positive in
        // screen coords).
        assert!(c.axis.y > 0.5, "axis should point down (+y), got {:?}", c.axis);
    }

    #[test]
    fn test_find_sharp_corners_star_has_five_tips() {
        // A 5-pointed sharp star has five outer tips (convex, ~36°
        // interior) and five inner concave valleys — only the outer
        // tips should be flagged as sharp convex corners.
        let path = star_path(200.0, 150.0, 5, 0.0);
        let corners = find_sharp_corners(&path, DEFAULT_SHARP_CORNER_ANGLE_DEG);
        assert_eq!(corners.len(), 5, "star should yield 5 outer tips, got {}", corners.len());
        // Each axis should point roughly radially outward from the
        // star's center.
        let cx = 100.0;
        let cy = 75.0;
        for c in &corners {
            let rx = c.position.x - cx;
            let ry = c.position.y - cy;
            let radial_len = (rx * rx + ry * ry).sqrt();
            if radial_len > 0.0 {
                // Dot product between outward axis and radial direction
                // should be positive (same hemisphere).
                let dot = (c.axis.x * rx + c.axis.y * ry) / radial_len;
                assert!(dot > 0.5, "axis not radially outward at {:?}: dot={dot}", c.position);
            }
        }
    }

    #[test]
    fn test_find_sharp_corners_rect_has_no_sharp_corners_at_60_threshold() {
        // Plain rectangle has 90° corners — above the 60° threshold.
        let path = rect_path(0.0, 0.0, 200.0, 150.0);
        let corners = find_sharp_corners(&path, DEFAULT_SHARP_CORNER_ANGLE_DEG);
        assert!(corners.is_empty(), "rectangle should not register any sharp corners at 60°, got {}", corners.len());
    }

    #[test]
    fn test_anchor_seeds_straddle_corner_axis() {
        // Anchors should sit symmetric across the outward axis and
        // equidistant from the corner.
        let boundary = rect_path(0.0, 0.0, 400.0, 300.0);
        let corner = SharpCorner {
            position: Point::new(100.0, 150.0),
            axis: Vec2::new(1.0, 0.0), // pointing +x
        };
        let (a, b) = anchor_seeds_for_corner(&corner, &boundary, 20.0).unwrap();
        // Midpoint of a and b should land on the axis line extending from corner.
        let mx = (a.x + b.x) * 0.5;
        let my = (a.y + b.y) * 0.5;
        assert!((my - 150.0).abs() < 1e-9, "midpoint.y != corner.y: {my}");
        assert!(mx > 100.0, "midpoint should be offset along +x axis, got {mx}");
        // Seeds are symmetric across the axis (same distance, opposite sides).
        assert!((a.x - b.x).abs() < 1e-9, "seeds should be at same x (symmetric across +x axis)");
        assert!(((a.y - 150.0) + (b.y - 150.0)).abs() < 1e-9, "seeds not symmetric across corner.y");
    }

    #[test]
    fn test_anchor_seeds_rotate_when_one_anchor_is_blocked() {
        // Carve a vertical slit out of the boundary directly above the
        // corner's outward axis: this is where one of the symmetric
        // anchors would land. The fallback should rotate the axis
        // until both anchors clear the slit, and the returned pair
        // must still be equidistant from the corner.
        use crate::masking::mask_difference;
        let outer = rect_path(0.0, 0.0, 400.0, 300.0);
        // 2×2 mm hole around the +y anchor's original position
        // (104, 152.4). Tight enough that rotating the axis away
        // moves both anchors clear of the hole.
        let slit = rect_path(103.0, 151.0, 2.0, 2.0);
        let boundary = mask_difference(&outer, &slit).unwrap();

        let corner = SharpCorner {
            position: Point::new(100.0, 150.0),
            axis: Vec2::new(1.0, 0.0),
        };
        let (a, b) = anchor_seeds_for_corner(&corner, &boundary, 20.0)
            .expect("rotation fallback should find a valid pair");

        // Both anchors must be in the boundary (winding != 0).
        use kurbo::Shape;
        assert!(boundary.winding(a) != 0, "anchor a outside boundary");
        assert!(boundary.winding(b) != 0, "anchor b outside boundary");

        // Equidistant from the corner — the structural property that
        // guarantees the bisector still passes through the corner.
        let da = ((a.x - 100.0).powi(2) + (a.y - 150.0).powi(2)).sqrt();
        let db = ((b.x - 100.0).powi(2) + (b.y - 150.0).powi(2)).sqrt();
        assert!((da - db).abs() < 1e-9, "anchors not equidistant: {da} vs {db}");
    }


    #[test]
    fn test_cvt_rectangular_boundary_produces_layout() {
        let boundary = rect_path(0.0, 0.0, 200.0, 150.0);
        let params = CvtParams {
            width: 200.0,
            height: 150.0,
            boundary: &boundary,
            piece_count: 20,
            seed: "cvt-rect",
            lloyd_iterations: 10,
            boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
            merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
            min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
            anchors: &[],
            min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
            smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
            cell_algorithm: crate::config::CellAlgorithm::Cvt,
        };
        let layout = build_cvt_layout(&params).expect("CVT build");
        assert_eq!(layout.pieces.len(), 20);
        assert!(!layout.edges.is_empty());
        for p in &layout.pieces {
            assert!(
                boundary.winding(p.center) != 0,
                "piece center {:?} drifted outside the boundary",
                p.center
            );
        }
    }

    #[test]
    fn test_cvt_heart_boundary_produces_pieces_inside() {
        let boundary = heart_path(200.0, 150.0);
        let params = CvtParams {
            width: 200.0,
            height: 150.0,
            boundary: &boundary,
            piece_count: 15,
            seed: "cvt-heart",
            lloyd_iterations: 15,
            boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
            merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
            min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
            anchors: &[],
            min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
            smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
            cell_algorithm: crate::config::CellAlgorithm::Cvt,
        };
        let layout = build_cvt_layout(&params).expect("CVT build");
        // Sliver-merge may absorb a piece at the heart's bottom tip.
        assert!(
            (14..=15).contains(&layout.pieces.len()),
            "expected 14–15 pieces after sliver merge, got {}",
            layout.pieces.len()
        );
        for p in &layout.pieces {
            assert!(
                boundary.winding(p.center) != 0,
                "piece {} center {:?} outside heart",
                p.id,
                p.center
            );
        }
    }

    #[test]
    fn test_cvt_deterministic() {
        let boundary = star_path(200.0, 150.0, 5, 0.0);
        let build = |seed: &str| -> PuzzleLayout {
            build_cvt_layout(&CvtParams {
                width: 200.0,
                height: 150.0,
                boundary: &boundary,
                piece_count: 12,
                seed,
                    lloyd_iterations: 10,
                boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
                merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
                min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
                anchors: &[],
                min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
                smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
                cell_algorithm: crate::config::CellAlgorithm::Cvt,
            })
            .expect("CVT build")
        };
        let a = build("same");
        let b = build("same");
        assert_eq!(a.pieces.len(), b.pieces.len());
        assert_eq!(a.edges.len(), b.edges.len());
        for (pa, pb) in a.pieces.iter().zip(b.pieces.iter()) {
            assert!((pa.center.x - pb.center.x).abs() < 1e-9);
            assert!((pa.center.y - pb.center.y).abs() < 1e-9);
        }
    }

    #[test]
    fn test_cvt_edges_reference_valid_pieces() {
        let boundary = rect_path(0.0, 0.0, 100.0, 100.0);
        let layout = build_cvt_layout(&CvtParams {
            width: 100.0,
            height: 100.0,
            boundary: &boundary,
            piece_count: 8,
            seed: "cvt-refs",
            lloyd_iterations: 10,
            boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
            merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
            min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
            anchors: &[],
            min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
            smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
            cell_algorithm: crate::config::CellAlgorithm::Cvt,
        })
        .unwrap();
        // Every edge must reference two valid, distinct pieces. Edge
        // connector presence is length-gated (short edges skip the
        // knob), so we only assert the connector / params are
        // *consistent* (both Some or both None).
        for e in &layout.edges {
            assert!(e.pieces.0 < layout.pieces.len());
            assert!(e.pieces.1 < layout.pieces.len());
            assert_ne!(e.pieces.0, e.pieces.1);
            assert_eq!(e.connector.is_some(), e.connector_params.is_some());
        }
        // At least some edges should get knobs in a 100×100 square with
        // 8 pieces (average cell edge well above threshold).
        assert!(
            layout.edges.iter().any(|e| e.connector.is_some()),
            "expected at least one knobbed edge"
        );
    }

    #[test]
    fn test_cvt_heart_48_pieces_spread_across_shape() {
        // Regression: 48 pieces in a heart should spread across the
        // whole heart (x: ~0..200, y: ~0..150), and most Delaunay
        // neighbours should be reported as real edges (not dropped by
        // clip_segment_to_polygon) — historically a bug that tried to
        // intersect against the curved BezPath directly dropped ~35% of
        // expected edges.
        let boundary = heart_path(200.0, 150.0);
        let layout = build_cvt_layout(&CvtParams {
            width: 200.0,
            height: 150.0,
            boundary: &boundary,
            piece_count: 48,
            seed: "heart-spread",
            lloyd_iterations: 25,
            boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
            merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
            min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
            anchors: &[],
            min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
            smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
            cell_algorithm: crate::config::CellAlgorithm::Cvt,
        })
        .unwrap();
        // Sliver-merge may absorb up to a couple of tip pieces at the
        // heart's bottom point, so we require "close to 48" rather than
        // exactly 48.
        assert!(
            (45..=48).contains(&layout.pieces.len()),
            "expected ~48 pieces after sliver merge, got {}",
            layout.pieces.len()
        );

        let x_min = layout
            .pieces
            .iter()
            .map(|p| p.center.x)
            .fold(f64::INFINITY, f64::min);
        let x_max = layout
            .pieces
            .iter()
            .map(|p| p.center.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let y_min = layout
            .pieces
            .iter()
            .map(|p| p.center.y)
            .fold(f64::INFINITY, f64::min);
        let y_max = layout
            .pieces
            .iter()
            .map(|p| p.center.y)
            .fold(f64::NEG_INFINITY, f64::max);
        // Width spread: piece centers should span most of the 0..200 range.
        assert!(x_min < 30.0 && x_max > 170.0, "x: [{x_min}, {x_max}]");
        // Height spread: at least 60 % of the 0..150 range (heart tapers
        // toward the bottom so we don't require 100 %).
        assert!(y_max - y_min > 90.0, "y: [{y_min}, {y_max}]");

        // Typical CVT: average neighbour count ≈ 6, so edge count for 48
        // cells should be in the 100-144 range. A major regression in
        // edge extraction would drop the count well below 100.
        assert!(
            layout.edges.len() >= 100,
            "too few edges extracted: {}",
            layout.edges.len()
        );
    }

    #[test]
    fn test_cvt_sampler_fails_on_low_fill_boundary() {
        // An L-shaped polygon whose interior covers ~0.2% of its own
        // bounding box. Rejection sampling burns through its attempt
        // budget before placing enough seeds, and the build returns an
        // error rather than hanging.
        let mut boundary = BezPath::new();
        boundary.move_to(Point::new(0.0, 0.0));
        boundary.line_to(Point::new(1000.0, 0.0));
        boundary.line_to(Point::new(1000.0, 1.0));
        boundary.line_to(Point::new(1.0, 1.0));
        boundary.line_to(Point::new(1.0, 1000.0));
        boundary.line_to(Point::new(0.0, 1000.0));
        boundary.close_path();
        let result = build_cvt_layout(&CvtParams {
            width: 1000.0,
            height: 1000.0,
            boundary: &boundary,
            piece_count: 5_000,
            seed: "cvt-sliver",
            lloyd_iterations: 0,
            boundary_flatten_tolerance: 0.1,
            merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
            min_knob_edge_length: DEFAULT_MIN_KNOB_EDGE_LENGTH,
            anchors: &[],
            min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
            smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
            cell_algorithm: crate::config::CellAlgorithm::Cvt,
        });
        let err = result
            .err()
            .expect("sampler should fail on thin-L + high piece count");
        assert!(err.contains("seeds") || err.contains("boundary"), "unexpected error: {err}");
    }
}
