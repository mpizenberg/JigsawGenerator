//! Algorithm-agnostic puzzle layout — the common contract every
//! piece-generation algorithm populates and every consumer reads.
//!
//! Rectangular grid, centroidal Voronoi, hex, Penrose … each becomes a
//! single `build_*_layout(config) -> PuzzleLayout` function. Connector
//! generation, SVG export, binary export, and the WASM bridge don't
//! branch on which algorithm produced the layout — they walk the same
//! `outer_boundary` + `edges` + `pieces` structure.
//!
//! Piece outlines are **derived**: the union of `outer_boundary` and the
//! `LayoutEdge` list fully defines every piece's cut path. Individual
//! piece contours can be assembled by walking a piece's `edge_indices`
//! in adjacency order, but the layout does not store them redundantly
//! (except for algorithms like CVT where the builder already has a
//! polygon in hand — see `LayoutPiece::outline`).

use std::collections::HashMap;

use kurbo::{
    BezPath, CubicBez, ParamCurve, ParamCurveArclen, ParamCurveNearest, PathEl, PathSeg, Point,
    Shape, Vec2,
};
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::binary_export::{CMD_CLOSE, CMD_CURVE_TO, CMD_LINE_TO, CMD_MOVE_TO};
use crate::connector::ConnectorGenerator;
use crate::edge::{EdgeParams, TabDirection};
use crate::svg_export::{build_svg_document, edge_transform};

/// A puzzle ready to be rendered or exported. Produced by a specific
/// builder (rectangular, CVT, …); consumed by the layout-agnostic export
/// functions below.
#[derive(Debug, Clone)]
pub struct PuzzleLayout {
    /// Puzzle width in mm (bounding rectangle width).
    pub width: f64,
    /// Puzzle height in mm.
    pub height: f64,
    /// Closed outer outline. Rectangular puzzles use the bounding
    /// rectangle; shape-bounded puzzles use the shape's contour.
    pub outer_boundary: BezPath,
    /// Internal edges between adjacent pieces, each a straight-line
    /// segment with an optional connector curve chain in edge-local
    /// coordinates.
    pub edges: Vec<LayoutEdge>,
    /// Piece metadata. Outlines are derived from edges + outer_boundary
    /// unless the builder cached a polygon on `LayoutPiece::outline`.
    pub pieces: Vec<LayoutPiece>,
    /// Maximum knob-`base` value (= `min(length, cross_length)`) any
    /// knob is allowed to use, computed from the pre-resolution
    /// average across interior edges. `0.0` disables the cap. Set by
    /// `compute_and_cap_avg_knob_base` after `generate_connectors`
    /// runs; used as a ceiling by `resolve_knob_collisions` and
    /// `knob_outer_boundary` so a single oversized edge can't produce
    /// a disproportionately large knob.
    pub knob_base_cap: f64,
}

/// One straight-line edge between two adjacent pieces. Carries its
/// connector curve chain (in edge-local coordinates, `x ∈ [0, length]`,
/// `y` perpendicular) and the params used to generate that connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutEdge {
    pub start: Point,
    pub end: Point,
    pub direction: TabDirection,
    pub connector: Option<Vec<CubicBez>>,
    pub connector_params: Option<EdgeParams>,
    /// Indices into `PuzzleLayout.pieces` for the two pieces that share
    /// this edge. Order is builder-defined and not otherwise significant.
    /// These are *post-merge* IDs — they get renumbered every time
    /// `merge_across_edges` runs.
    pub pieces: (usize, usize),
}

impl LayoutEdge {
    /// Euclidean length of the underlying straight-line segment in mm.
    pub fn length(&self) -> f64 {
        (self.end - self.start).hypot()
    }
}

/// Metadata for one puzzle piece. Outline is optional — builders that
/// compute piece polygons directly (CVT cells) can cache them here;
/// builders that don't (rectangular) leave `outline = None` and
/// consumers derive outlines from edges + outer boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutPiece {
    pub id: usize,
    pub center: Point,
    /// Indices into `PuzzleLayout.edges` for every edge bounding this
    /// piece. Excludes outer-boundary segments.
    pub edge_indices: Vec<usize>,
    /// Optional closed piece outline (in global coords). Builders that
    /// already have the polygon set this; otherwise it's derived by
    /// consumers when needed.
    pub outline: Option<BezPath>,
}

impl PuzzleLayout {
    /// Populate `connector` and `connector_params` on every internal
    /// edge using the given generator. The `cross_length` each edge sees
    /// is the straight-line distance between the two adjacent piece
    /// *centers* — for rectangular grids that recovers the perpendicular
    /// cell dimension exactly, and for CVT it's the natural "width" of
    /// the neighborhood straddling the edge. Deterministic given `seed`;
    /// same seed string → identical connectors.
    ///
    /// `min_knob_edge_length` (mm) is the minimum edge length below which
    /// the edge gets no knob at all — `connector` stays `None` and the
    /// edge renders as a straight line. Pass `0.0` to always generate a
    /// knob regardless of edge length.
    pub fn generate_connectors(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
        seed: &str,
        min_knob_edge_length: f64,
    ) {
        self.generate_connectors_with_style(connector, seed, min_knob_edge_length, 1.0, 0.0);
    }

    /// Generate the same shared cuts with seeded placement variation and
    /// a size multiplier retained through collision resolution.
    pub fn generate_connectors_with_style(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
        seed: &str,
        min_knob_edge_length: f64,
        knob_scale: f64,
        randomness: f64,
    ) {
        let cross_lengths: Vec<f64> = self
            .edges
            .iter()
            .map(|edge| {
                let a = self.pieces[edge.pieces.0].center;
                let b = self.pieces[edge.pieces.1].center;
                (a - b).hypot()
            })
            .collect();

        // Flatten the outer boundary once for all O(edges) safety
        // probes; build the Y-bucket index alongside so each probe's
        // polygon_contains is O(1) average instead of O(N segments).
        let boundary_polygon =
            crate::flat_boundary::flatten_polygon(&self.outer_boundary, 0.25);
        let boundary_index =
            crate::flat_boundary::BoundaryIndex::new(&boundary_polygon);

        let mut rng = crate::seed::create_rng(&format!("{}-connectors", seed));
        let mut position_rng = crate::seed::create_rng(&format!("{}-knob-positions", seed));
        for (edge, cross_length) in self.edges.iter_mut().zip(cross_lengths) {
            let length = edge.length();
            if length < min_knob_edge_length {
                edge.connector = None;
                edge.connector_params = None;
                continue;
            }

            // Probe both perpendicular sides at the knob's silhouette
            // (center tip + two shoulders). If exactly one side would
            // push the knob past the boundary (outer contour *or* a
            // whimsy hole), force direction to the other side. If both
            // sides would overlap, skip the knob entirely.
            let base = length.min(cross_length) * knob_scale;
            let room = (length * 0.5 - base * 0.25 * 1.2).max(0.0);
            let offset = position_rng.random_range(-1.0..1.0) * randomness * room.min(length * 0.18);
            let (out_safe, in_safe) = knob_safety_probe_with_style(
                edge.start,
                edge.end,
                cross_length,
                knob_scale,
                offset,
                &boundary_polygon,
                &boundary_index,
            );
            match (out_safe, in_safe) {
                (true, false) => edge.direction = TabDirection::Out,
                (false, true) => edge.direction = TabDirection::In,
                (false, false) => {
                    edge.connector = None;
                    edge.connector_params = None;
                    continue;
                }
                _ => {}
            }

            let params = EdgeParams {
                length,
                cross_length,
                direction: edge.direction,
                offset,
                knob_scale,
            };
            let curves = connector.generate(&params, &mut rng);
            edge.connector = Some(curves);
            edge.connector_params = Some(params);
        }
    }

    /// Walk every edge with a connector and ensure its knob doesn't
    /// collide with a neighbor knob (an edge that shares a piece).
    /// Resolution proceeds in three phases, each preserving as much
    /// of the original knob shape as possible:
    ///
    /// 1. **Flip pass** — for each colliding knob, flip
    ///    `TabDirection` if the flipped side passes the boundary
    ///    safety probe and the flip resolves the conflict. Cheapest
    ///    fix; no shape, size, or position change.
    /// 2. **Slide pass** — bounded iteration (max
    ///    `MAX_SLIDE_SWEEPS`). Each sweep slides every still-
    ///    colliding knob along its own edge in a direction that
    ///    pushes it away from its conflicting neighbor. Slide
    ///    distance per sweep is bounded by `SLIDE_STEP_FRACTION ×
    ///    knob_w` and total displacement is capped at the edge's
    ///    physical room (so the knob can't slide off its edge).
    ///    Vertex-shared edges slide away from the shared vertex;
    ///    parallel edges fall through to the next phase since
    ///    sliding along the edge axis can't reduce a perpendicular
    ///    gap. Bounded sweeps + bounded steps prevents unbounded
    ///    cascades.
    /// 3. **Component shrink** — anything still colliding is dumped
    ///    into a conflict graph (edges = remaining overlapping
    ///    pairs), connected components are extracted via union-find,
    ///    and each component is uniformly scaled down to the largest
    ///    factor that resolves all internal conflicts. Scaling is
    ///    monotonic — a smaller knob has a smaller shell, which can
    ///    only reduce overlap counts — so this never undoes phase 1
    ///    or 2's work.
    ///
    /// OBB-vs-OBB collision test uses SAT with `clearance_mm` of
    /// padding on every side as the buffer-shell radius. Should be
    /// called *after* `generate_connectors`.
    pub fn resolve_knob_collisions(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
        clearance_mm: f64,
        extra_obstacles: &[KnobObb],
    ) {
        // Always include obstacles built from any *inner* subpath of
        // `outer_boundary` (whimsy holes). Each polyline segment of
        // each hole becomes a thin OBB so knob shells maintain
        // `clearance_mm` distance from the whimsy contour. Combined
        // with `extra_obstacles` (e.g. boundary knobs from
        // `knob_outer_boundary`) into one fixed-obstacle list used by
        // every phase below.
        let hole_obstacles = boundary_hole_obstacles(&self.outer_boundary);
        let fixed_obstacles_owned: Vec<KnobObb> = extra_obstacles
            .iter()
            .copied()
            .chain(hole_obstacles)
            .collect();
        let fixed_obstacles: &[KnobObb] = &fixed_obstacles_owned;
        // Flatten the outer boundary + Y-bucket index once for all
        // knob-safety probes across the flip / slide / shrink phases
        // below. Same index used for every per-edge probe.
        let boundary_polygon =
            crate::flat_boundary::flatten_polygon(&self.outer_boundary, 0.25);
        let boundary_index =
            crate::flat_boundary::BoundaryIndex::new(&boundary_polygon);
        /// Number of slide-phase sweeps. Each sweep re-samples every
        /// colliding knob's optimal offset given the current state of
        /// its neighbors; subsequent sweeps converge if a previous
        /// slide changed a neighbor's position.
        const MAX_SLIDE_SWEEPS: usize = 3;
        /// Offset fractions sampled per knob (relative to the maximum
        /// physically allowed slide distance for that edge). Includes
        /// both extremes (`±1.0`) so we always explore "all the way
        /// each direction" before settling on an interior balance
        /// point.
        const SLIDE_SAMPLE_FRACTIONS: [f64; 9] =
            [-1.0, -0.7, -0.4, -0.2, 0.0, 0.2, 0.4, 0.7, 1.0];
        /// Component-shrink scale ladder — finest first. The largest
        /// k that resolves every internal conflict in a component is
        /// the one we apply.
        const SHRINK_SCALES: [f64; 6] = [1.0, 0.85, 0.70, 0.55, 0.40, 0.25];

        // Snapshot original params per edge — every slide/shrink
        // attempt rebuilds from these (length and cross_length stay
        // canonical; direction, offset, and an effective scale vary).
        let original_params: Vec<Option<EdgeParams>> = self
            .edges
            .iter()
            .map(|e| e.connector_params.clone())
            .collect();

        let mut obbs: Vec<Option<KnobObb>> = (0..self.edges.len())
            .map(|i| knob_global_obb(&self.edges[i], clearance_mm))
            .collect();

        // Neighbor sets per edge: edges sharing at least one of the
        // two adjacent pieces. Only these can collide.
        let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); self.edges.len()];
        for piece in &self.pieces {
            for &i in &piece.edge_indices {
                for &j in &piece.edge_indices {
                    if i != j {
                        neighbors[i].push(j);
                    }
                }
            }
        }
        for ns in &mut neighbors {
            ns.sort_unstable();
            ns.dedup();
        }

        let interior_conflicts =
            |idx: usize, obbs: &[Option<KnobObb>]| -> Vec<usize> {
                let Some(my) = obbs[idx].as_ref() else { return Vec::new() };
                neighbors[idx]
                    .iter()
                    .copied()
                    .filter(|&j| obbs[j].as_ref().is_some_and(|nb| obbs_overlap(my, nb)))
                    .collect()
            };
        let fixed_conflict_centers =
            |idx: usize, obbs: &[Option<KnobObb>]| -> Vec<Point> {
                let Some(my) = obbs[idx].as_ref() else { return Vec::new() };
                fixed_obstacles
                    .iter()
                    .filter(|fobb| obbs_overlap(my, fobb))
                    .map(|fobb| {
                        // OBB "center" = average of its 4 corners.
                        let mut x = 0.0;
                        let mut y = 0.0;
                        for c in fobb.corners.iter() {
                            x += c.x;
                            y += c.y;
                        }
                        Point::new(x * 0.25, y * 0.25)
                    })
                    .collect()
            };
        let collides_at = |idx: usize, obbs: &[Option<KnobObb>]| -> bool {
            !interior_conflicts(idx, obbs).is_empty()
                || !fixed_conflict_centers(idx, obbs).is_empty()
        };

        // ─── Phase 1: flip ─────────────────────────────────────────
        for i in 0..self.edges.len() {
            if self.edges[i].connector.is_none() {
                continue;
            }
            let Some(orig) = original_params[i].clone() else {
                continue;
            };
            if !collides_at(i, &obbs) {
                continue;
            }
            let (out_safe, in_safe) = knob_safety_probe_with_style(
                self.edges[i].start,
                self.edges[i].end,
                orig.cross_length,
                orig.knob_scale,
                orig.offset,
                &boundary_polygon,
                &boundary_index,
            );
            let flipped = match orig.direction {
                TabDirection::Out => TabDirection::In,
                TabDirection::In => TabDirection::Out,
            };
            let flipped_safe = match flipped {
                TabDirection::Out => out_safe,
                TabDirection::In => in_safe,
            };
            if !flipped_safe {
                continue;
            }
            // Build the flipped OBB analytically and probe it before
            // touching the connector. Avoids two `connector.generate()`
            // calls per flip-attempt (flip → maybe-revert) — the OBB
            // is purely geometric, no bezier curves required.
            let flipped_params = EdgeParams {
                length: orig.length,
                cross_length: orig.cross_length,
                direction: flipped,
                offset: orig.offset,
                knob_scale: orig.knob_scale,
            };
            let flipped_obb = obb_from_chord(
                self.edges[i].start,
                self.edges[i].end,
                &flipped_params,
                clearance_mm,
            );
            let prev = obbs[i].replace(flipped_obb);
            if collides_at(i, &obbs) {
                // Flip didn't help — restore the previous OBB; the
                // connector was never modified.
                obbs[i] = prev;
            } else {
                // Flip helps — commit it once.
                apply_knob_variant(
                    &mut self.edges[i],
                    &orig,
                    flipped,
                    1.0,
                    orig.offset,
                    connector,
                );
            }
        }

        // ─── Phase 2: balanced slide ───────────────────────────────
        // For each colliding knob, sample several offset positions
        // along its edge (both extremes plus interior points) and
        // pick the one that minimizes the *worst* OBB-penetration
        // depth across all conflicting neighbors. Two outcomes:
        //   - If some sample reaches zero penetration, slide alone
        //     resolves the conflict and the knob keeps full size.
        //   - Otherwise, the chosen offset is the "balance point"
        //     where conflicts on either side are roughly equal —
        //     that's the right starting position for the shrink
        //     phase, so when we shrink uniformly both sides clear at
        //     the same scale.
        // Iterating the sweep a few times lets neighbors settle: if
        // knob A slides, knob B's optimal offset may shift.
        for _ in 0..MAX_SLIDE_SWEEPS {
            let mut any_change = false;
            for i in 0..self.edges.len() {
                if self.edges[i].connector.is_none() {
                    continue;
                }
                let Some(orig) = original_params[i].clone() else {
                    continue;
                };
                let curr = self.edges[i].connector_params.clone().unwrap_or(orig.clone());
                let knob_w = curr.length.min(curr.cross_length) * curr.knob_scale * 0.25;
                let styled = orig.knob_scale != 1.0 || orig.offset != 0.0;
                let approach = if styled { 1.2 } else { 1.0 };
                let max_offset = (curr.length * 0.5 - knob_w * approach).max(0.0);
                if max_offset < 1e-9 {
                    continue;
                }
                if !collides_at(i, &obbs) {
                    continue;
                }

                // Score each candidate offset by max-penetration
                // depth against all conflict sources. Smallest depth
                // wins (zero = fully resolved).
                let scale = curr.cross_length / orig.cross_length;
                let mut best_offset = curr.offset;
                let mut best_depth = f64::INFINITY;
                for &frac in &SLIDE_SAMPLE_FRACTIONS {
                    let offset = frac * max_offset;
                    let trial_params = EdgeParams {
                        length: orig.length,
                        cross_length: orig.cross_length * scale,
                        direction: curr.direction,
                        offset,
                        knob_scale: orig.knob_scale,
                    };
                    if styled {
                        let (out_safe, in_safe) = knob_safety_probe_with_style(
                            self.edges[i].start, self.edges[i].end,
                            trial_params.cross_length, trial_params.knob_scale, offset,
                            &boundary_polygon, &boundary_index,
                        );
                        if !match curr.direction { TabDirection::Out => out_safe, TabDirection::In => in_safe } {
                            continue;
                        }
                    }
                    let trial_obb = obb_from_chord(
                        self.edges[i].start,
                        self.edges[i].end,
                        &trial_params,
                        clearance_mm,
                    );
                    let mut max_depth = 0.0_f64;
                    for &j in &neighbors[i] {
                        if let Some(nb) = obbs[j].as_ref() {
                            let d = obb_penetration(&trial_obb, nb);
                            if d > max_depth {
                                max_depth = d;
                            }
                        }
                    }
                    for fobb in fixed_obstacles {
                        let d = obb_penetration(&trial_obb, fobb);
                        if d > max_depth {
                            max_depth = d;
                        }
                    }
                    if max_depth < best_depth {
                        best_depth = max_depth;
                        best_offset = offset;
                    }
                }

                if (best_offset - curr.offset).abs() > 1e-9 {
                    apply_knob_variant(
                        &mut self.edges[i],
                        &orig,
                        curr.direction,
                        scale,
                        best_offset,
                        connector,
                    );
                    obbs[i] = knob_global_obb(&self.edges[i], clearance_mm);
                    any_change = true;
                }
            }
            if !any_change {
                break;
            }
        }

        // ─── Phase 3: component shrink ─────────────────────────────
        // Build conflict graph: pairs of *interior* knobs whose
        // shells still overlap, plus a "phantom" graph hop for any
        // interior knob that conflicts with a fixed obstacle (we use
        // a dummy index `usize::MAX` to denote "anchored to a fixed
        // obstacle").
        let mut conflict_pairs: Vec<(usize, usize)> = Vec::new();
        let mut conflicts_with_fixed: Vec<bool> = vec![false; self.edges.len()];
        for i in 0..self.edges.len() {
            if obbs[i].is_some() && !fixed_conflict_centers(i, &obbs).is_empty() {
                conflicts_with_fixed[i] = true;
            }
            for &j in &neighbors[i] {
                if j <= i {
                    continue;
                }
                let (Some(a), Some(b)) = (obbs[i].as_ref(), obbs[j].as_ref()) else {
                    continue;
                };
                if obbs_overlap(a, b) {
                    conflict_pairs.push((i, j));
                }
            }
        }
        let any_fixed = conflicts_with_fixed.iter().any(|&b| b);
        if conflict_pairs.is_empty() && !any_fixed {
            return;
        }

        // Union-find on interior-interior conflict pairs. Interior
        // knobs that only conflict with a fixed obstacle (no
        // interior neighbor in the conflict graph) form their own
        // singleton component.
        let mut uf = UnionFind::new(self.edges.len());
        for &(a, b) in &conflict_pairs {
            uf.union(a, b);
        }
        let mut components: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for &(a, _) in &conflict_pairs {
            components.entry(uf.find(a)).or_default().push(a);
        }
        for &(_, b) in &conflict_pairs {
            let root = uf.find(b);
            let entry = components.entry(root).or_default();
            if !entry.contains(&b) {
                entry.push(b);
            }
        }
        // Any interior knob that conflicts only with a fixed obstacle
        // gets its own singleton component so shrink can address it.
        for (i, &has) in conflicts_with_fixed.iter().enumerate() {
            if has {
                let root = uf.find(i);
                let entry = components.entry(root).or_default();
                if !entry.contains(&i) {
                    entry.push(i);
                }
            }
        }

        for (_, indices) in components {
            // Find the largest scale where every internal conflict
            // pair AND every interior-vs-fixed conflict in the
            // component is collision-free. SHRINK_SCALES is
            // largest-first; we walk it and apply the first that works.
            //
            // At each scale we *also* re-evaluate direction per
            // knob — the flipped side that was unsafe at full size
            // (e.g. would push into a whimsy) often becomes safe
            // once the knob shrinks. Without this, a knob that
            // failed phase 1's flip stays on the original side and
            // the shrink phase can't escape that choice.
            for &k in &SHRINK_SCALES {
                for &i in &indices {
                    let Some(orig) = original_params[i].clone() else {
                        continue;
                    };
                    let curr = self
                        .edges[i]
                        .connector_params
                        .clone()
                        .unwrap_or(orig.clone());
                    let scaled_cross = orig.cross_length * k;
                    let (out_safe, in_safe) = knob_safety_probe_with_style(
                        self.edges[i].start,
                        self.edges[i].end,
                        scaled_cross,
                        orig.knob_scale,
                        if orig.knob_scale == 1.0 && orig.offset == 0.0 { 0.0 } else { curr.offset },
                        &boundary_polygon,
                        &boundary_index,
                    );
                    // Pick the direction with the lowest total
                    // overlap depth among boundary-safe options at
                    // this scale.
                    let mut best_dir = curr.direction;
                    let mut best_depth = f64::INFINITY;
                    for &dir in &[TabDirection::Out, TabDirection::In] {
                        let safe = match dir {
                            TabDirection::Out => out_safe,
                            TabDirection::In => in_safe,
                        };
                        if !safe {
                            continue;
                        }
                        let trial_params = EdgeParams {
                            length: orig.length,
                            cross_length: scaled_cross,
                            direction: dir,
                            offset: curr.offset,
                            knob_scale: orig.knob_scale,
                        };
                        let trial_obb = obb_from_chord(
                            self.edges[i].start,
                            self.edges[i].end,
                            &trial_params,
                            clearance_mm,
                        );
                        let mut depth = 0.0_f64;
                        for &j in &neighbors[i] {
                            if let Some(nb) = obbs[j].as_ref() {
                                let d = obb_penetration(&trial_obb, nb);
                                if d > depth {
                                    depth = d;
                                }
                            }
                        }
                        for fobb in fixed_obstacles {
                            let d = obb_penetration(&trial_obb, fobb);
                            if d > depth {
                                depth = d;
                            }
                        }
                        if depth < best_depth {
                            best_depth = depth;
                            best_dir = dir;
                        }
                    }
                    apply_knob_variant(
                        &mut self.edges[i],
                        &orig,
                        best_dir,
                        k,
                        curr.offset,
                        connector,
                    );
                    obbs[i] = knob_global_obb(&self.edges[i], clearance_mm);
                }
                let interior_clear = indices.iter().enumerate().all(|(ai, &i)| {
                    indices.iter().skip(ai + 1).all(|&j| {
                        let (Some(oa), Some(ob)) = (obbs[i].as_ref(), obbs[j].as_ref()) else {
                            return true;
                        };
                        !obbs_overlap(oa, ob)
                    })
                });
                let fixed_clear = indices.iter().all(|&i| {
                    let Some(my) = obbs[i].as_ref() else { return true };
                    fixed_obstacles.iter().all(|fobb| !obbs_overlap(my, fobb))
                });
                if interior_clear && fixed_clear {
                    break;
                }
            }
        }
    }

    /// Recheck customized profiles against the actual outline and whimsy
    /// holes after collision resolution has moved or flipped them. Shrink
    /// a profile locally if necessary; never alter the underlying mesh.
    pub fn fit_styled_connectors_to_boundary(&mut self) {
        let boundary = &self.outer_boundary;
        let mut rng = crate::seed::create_rng("knob-boundary-fit");
        for edge in &mut self.edges {
            let Some(mut params) = edge.connector_params.clone() else { continue; };
            let transform = edge_transform(edge.start, edge.end);
            let mut fits = false;
            for _ in 0..32 {
                let curves = edge.connector.as_ref().unwrap();
                fits = curves.iter().all(|curve| (1..16).all(|i| {
                    let point = transform * curve.eval(i as f64 / 16.0);
                    boundary.winding(point) != 0
                }));
                if fits { break; }
                params.knob_scale *= 0.9;
                edge.connector = Some(crate::ClassicKnobConnector.generate(&params, &mut rng));
            }
            if fits {
                edge.connector_params = Some(params);
            } else {
                edge.connector = None;
                edge.connector_params = None;
            }
        }
    }

    /// Remove connector curves from edges whose length is small
    /// relative to the layout's own edge-length distribution, while
    /// guaranteeing every piece keeps at least `min_knobs_per_piece`
    /// knobbed edges.
    ///
    /// `ratio` is the fraction of the **median** edge length below
    /// which an edge becomes a removal candidate. Using the median (not
    /// mean) keeps the threshold stable against a handful of very long
    /// edges (e.g. a star-tip edge that dwarfs the rest).
    ///
    /// Strategy: sort candidates shortest-first, greedily remove each
    /// candidate's connector *only* if both adjacent pieces would
    /// retain at least `min_knobs_per_piece` connectors afterwards. A
    /// piece that's about to drop below the minimum keeps its shortest
    /// surviving candidate knob; the longer of two candidates on the
    /// same piece is preferred to survive because it renders more
    /// clearly.
    ///
    /// Idempotent; safe to call multiple times. Should be called
    /// **after** `generate_connectors`.
    pub fn remove_small_knobs(&mut self, ratio: f64, min_knobs_per_piece: usize) {
        if self.edges.is_empty() {
            return;
        }

        // Compute median length across every edge (not just the
        // knobbed ones) — a just-generated layout has a knob on every
        // edge anyway, and we want the median of the *geometry*.
        let mut lengths: Vec<f64> = self.edges.iter().map(|e| e.length()).collect();
        lengths.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = lengths[lengths.len() / 2];
        let threshold = median * ratio;

        // Candidates: edges shorter than threshold that currently carry
        // a connector. Sort shortest-first so we try the worst
        // offenders before the marginal ones.
        let mut candidates: Vec<usize> = self
            .edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                (e.connector.is_some() && e.length() < threshold).then_some(i)
            })
            .collect();
        candidates.sort_by(|&a, &b| {
            self.edges[a]
                .length()
                .partial_cmp(&self.edges[b].length())
                .unwrap()
        });

        // Per-piece count of edges that still have a connector. We'll
        // decrement as removals happen to keep the floor constraint
        // live.
        let mut knob_count: Vec<usize> = self
            .pieces
            .iter()
            .map(|p| {
                p.edge_indices
                    .iter()
                    .filter(|&&i| self.edges[i].connector.is_some())
                    .count()
            })
            .collect();

        for edge_idx in candidates {
            let (pa, pb) = self.edges[edge_idx].pieces;
            if knob_count[pa] > min_knobs_per_piece
                && knob_count[pb] > min_knobs_per_piece
            {
                self.edges[edge_idx].connector = None;
                self.edges[edge_idx].connector_params = None;
                knob_count[pa] -= 1;
                knob_count[pb] -= 1;
            }
        }
    }

    /// Merge piece pairs whose shared internal edge is shorter than
    /// `threshold` mm. Short edges typically come from CVT seeds
    /// straddling a narrow section of the shape: the Voronoi edge
    /// between them runs perpendicular to the narrow axis and ends up
    /// too small to host a useful connector. Merging folds those
    /// adjacent cells into a single piece whose boundary passes through
    /// the narrow section cleanly.
    ///
    /// Uses union-find so a chain of consecutive short edges — e.g.
    /// four seeds in a row along a thin strip — collapses into one
    /// piece in a single call. Piece IDs are renumbered `0..new_len`;
    /// `edge.pieces` and `piece.edge_indices` are remapped accordingly.
    ///
    /// Should be called *before* [`generate_connectors`]: merging
    /// removes edges from the layout, and the remaining edges keep
    /// whatever `connector_params` they already had.
    /// Rebuild the outer subpath of `outer_boundary` so each segment
    /// between consecutive **outer-edge vertices** becomes one outer
    /// edge with a classic knob centered on it. Visually, every cell
    /// along the boundary gets a knob on the side that faces
    /// nothingness — the puzzle silhouette interlocks with itself the
    /// same way interior pieces interlock with each other.
    ///
    /// Two kinds of vertices contribute, both treated identically:
    /// - **Voronoi clip points**: edge endpoints with shared-edge
    ///   degree < 3 — bisector tips that landed on the boundary.
    /// - **Boundary corners**: junctions in the original outer-path
    ///   where the tangent direction changes by more than
    ///   `CORNER_TURN_THRESHOLD_DEG`. A rectangle's four 90° corners,
    ///   a heart's bottom tip, and a star's ten tips/valleys all
    ///   qualify; a circle's smooth curve does not.
    ///
    /// Including corners prevents chords from cutting across a hard
    /// turn — e.g. on a rectangle, a clip point near the top-right
    /// corner on the top edge and one on the right edge would
    /// otherwise be connected by a diagonal chord that lops off the
    /// corner.
    ///
    /// Algorithm:
    /// 1. Find all vertices (clip points + corners) on the outer
    ///    subpath.
    /// 2. Project each onto the outer subpath; drop ones too far
    ///    (they belong to whimsy holes).
    /// 3. Sort by arc-length and dedupe near-coincident vertices.
    /// 4. Walk consecutive pairs; emit a chord-with-knob per pair.
    /// 5. Append inner subpaths (whimsy holes) unchanged.
    pub fn knob_outer_boundary(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
        seed: &str,
    ) {
        self.knob_outer_boundary_with_style(connector, seed, 1.0, 0.0);
    }

    pub fn knob_outer_boundary_with_style(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
        seed: &str,
        knob_scale: f64,
        randomness: f64,
    ) {
        let clearance_mm = crate::cvt::knob_clearance_for_base(self.knob_base_cap);
        // Tangent-direction change at a path-segment junction beyond
        // which we treat that junction as an outer-edge vertex. 30°
        // is below a hexagon's 60° turns and well below a rectangle's
        // 90° — but high enough that a near-flat smooth-curve junction
        // (heart side, circle quarter-arc) doesn't trip it.
        const CORNER_TURN_THRESHOLD_DEG: f64 = 30.0;
        // Distance below which we consider two vertices coincident
        // and dedupe — clip points sometimes land essentially on a
        // corner.
        const VERTEX_DEDUPE_MM: f64 = 0.5;
        const OUTER_PROJECT_TOL: f64 = 1.0;
        const TOL: f64 = 1e-6;

        if self.edges.is_empty() {
            return;
        }
        // Split outer_boundary into subpaths. The first one is the
        // outer ring; the rest are whimsy holes (CW relative to the
        // outer's winding).
        let subpaths = split_subpaths(&self.outer_boundary);
        if subpaths.is_empty() {
            return;
        }
        let outer = &subpaths[0];
        let outer_segments: Vec<PathSeg> = outer.segments().collect();
        if outer_segments.is_empty() {
            return;
        }
        let outer_arc_lens: Vec<f64> = outer_segments.iter().map(|s| s.arclen(0.1)).collect();

        // Cumulative arc length up to (but not including) each segment.
        let mut cum_arc: Vec<f64> = Vec::with_capacity(outer_arc_lens.len() + 1);
        cum_arc.push(0.0);
        let mut acc = 0.0;
        for &l in &outer_arc_lens {
            acc += l;
            cum_arc.push(acc);
        }
        let perimeter = acc;

        // ─── Vertices from corners in the path ──────────────────────
        // For each segment-to-segment junction, measure the tangent
        // direction change. Above the threshold ⇒ that junction is a
        // corner and the start of segment `i` is an outer-edge vertex.
        let corner_threshold = CORNER_TURN_THRESHOLD_DEG.to_radians();
        let mut vertices: Vec<(f64, Point)> = Vec::new();
        for i in 0..outer_segments.len() {
            let prev_idx = (i + outer_segments.len() - 1) % outer_segments.len();
            let t_out = segment_end_tangent(&outer_segments[prev_idx]);
            let t_in = segment_start_tangent(&outer_segments[i]);
            let lo = t_out.hypot();
            let li = t_in.hypot();
            if lo < 1e-9 || li < 1e-9 {
                continue;
            }
            let cos = ((t_out.x * t_in.x + t_out.y * t_in.y) / (lo * li)).clamp(-1.0, 1.0);
            let angle_change = cos.acos();
            if angle_change >= corner_threshold {
                vertices.push((cum_arc[i], segment_start_point(&outer_segments[i])));
            }
        }

        // ─── Vertices from Voronoi clip points ──────────────────────
        // Edge endpoints with shared-edge degree < 3 are clipped
        // bisector tips on the boundary.
        let mut verts: Vec<Point> = Vec::new();
        let find_or_add = |p: Point, verts: &mut Vec<Point>| -> usize {
            for (i, v) in verts.iter().enumerate() {
                if (v.x - p.x).abs() < TOL && (v.y - p.y).abs() < TOL {
                    return i;
                }
            }
            verts.push(p);
            verts.len() - 1
        };
        let mut endpoints: Vec<(usize, usize)> = Vec::with_capacity(self.edges.len());
        for edge in &self.edges {
            let a = find_or_add(edge.start, &mut verts);
            let b = find_or_add(edge.end, &mut verts);
            endpoints.push((a, b));
        }
        let mut degree = vec![0usize; verts.len()];
        for &(a, b) in &endpoints {
            degree[a] += 1;
            degree[b] += 1;
        }

        for (vi, v) in verts.iter().enumerate() {
            if degree[vi] >= 3 {
                continue; // interior Voronoi vertex
            }
            // Project the clip point onto the outer subpath; drop
            // ones too far (they belong to whimsy holes).
            let mut best_seg = 0usize;
            let mut best_t = 0.0;
            let mut best_dist_sq = f64::INFINITY;
            for (si, seg) in outer_segments.iter().enumerate() {
                let n = seg.nearest(*v, 0.01);
                if n.distance_sq < best_dist_sq {
                    best_dist_sq = n.distance_sq;
                    best_t = n.t;
                    best_seg = si;
                }
            }
            if best_dist_sq.sqrt() > OUTER_PROJECT_TOL {
                continue;
            }
            let partial = if outer_arc_lens[best_seg] > 1e-9 {
                outer_segments[best_seg]
                    .subsegment(0.0..best_t)
                    .arclen(0.01)
            } else {
                0.0
            };
            vertices.push((cum_arc[best_seg] + partial, *v));
        }

        if vertices.len() < 3 {
            return;
        }

        vertices.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Dedupe near-coincident vertices — a clip point landing
        // right on a corner would otherwise produce a zero-length
        // chord. Compare against next neighbor and the wraparound
        // back to the first.
        let mut deduped: Vec<(f64, Point)> = Vec::with_capacity(vertices.len());
        for v in vertices {
            if deduped
                .last()
                .is_some_and(|last| (v.1 - last.1).hypot() < VERTEX_DEDUPE_MM)
            {
                continue;
            }
            deduped.push(v);
        }
        if let (Some(first), Some(last)) = (deduped.first().copied(), deduped.last().copied())
            && deduped.len() >= 2
        {
            // Wraparound: treat first and last as adjacent in the
            // closed loop. If they're close, drop the duplicate.
            let direct = (first.0 - last.0).abs();
            let wrap = perimeter - direct;
            let wrap_dist = direct.min(wrap);
            if (first.1 - last.1).hypot() < VERTEX_DEDUPE_MM || wrap_dist < VERTEX_DEDUPE_MM {
                deduped.pop();
            }
        }
        let clips_with_arc = deduped;

        if clips_with_arc.len() < 3 {
            return;
        }

        // Build new outer subpath: chord-with-knob for every
        // consecutive pair of clip points, closing back to the first.
        // Also accumulate one `KnobObb` per boundary knob; these
        // become *fixed obstacles* for the subsequent
        // `resolve_knob_collisions` pass so interior knobs flip /
        // slide / shrink to avoid the (usually large) boundary knobs.
        let n = clips_with_arc.len();
        let mut new_outer = BezPath::new();
        new_outer.move_to(clips_with_arc[0].1);
        let mut boundary_obbs: Vec<KnobObb> = Vec::with_capacity(n);
        let mut rng = crate::seed::create_rng(&format!("{}-outer-knobs", seed));
        for i in 0..n {
            let p_a = clips_with_arc[i].1;
            let p_b = clips_with_arc[(i + 1) % n].1;
            let chord_len = (p_b - p_a).hypot();
            if chord_len < 1e-6 {
                continue;
            }
            // Knob proportions: use chord length as both length and
            // cross_length, but cap `cross_length` at the layout's
            // pre-resolution average knob-base so outer-edge knobs
            // can't dwarf their interior peers. Direction is chosen
            // per-knob from the outer-knob RNG stream — `In` bumps
            // outward (away from puzzle interior) for our CW shape
            // library, `Out` bumps inward (carving a notch into the
            // silhouette).
            let direction = if rng.random_bool(0.5) {
                TabDirection::In
            } else {
                TabDirection::Out
            };
            let cross = if self.knob_base_cap > 0.0 {
                chord_len.min(self.knob_base_cap / knob_scale)
            } else {
                chord_len
            };
            let params = EdgeParams {
                length: chord_len,
                cross_length: cross,
                direction,
                offset: if randomness == 0.0 { 0.0 } else {
                    let room = (chord_len * 0.5 - chord_len.min(cross) * knob_scale * 0.3).max(0.0);
                    rng.random_range(-1.0..1.0) * randomness * room.min(chord_len * 0.18)
                },
                knob_scale,
            };
            let curves = connector.generate(&params, &mut rng);
            let aff = crate::svg_export::edge_transform(p_a, p_b);
            for curve in &curves {
                let p1 = aff * curve.p1;
                let p2 = aff * curve.p2;
                let p3 = aff * curve.p3;
                new_outer.curve_to(p1, p2, p3);
            }
            boundary_obbs.push(obb_from_chord(p_a, p_b, &params, clearance_mm));
        }
        new_outer.close_path();

        // Re-attach inner subpaths (whimsy holes) to the new outer.
        for sub in &subpaths[1..] {
            for el in sub.iter() {
                match el {
                    PathEl::MoveTo(p) => new_outer.move_to(p),
                    PathEl::LineTo(p) => new_outer.line_to(p),
                    PathEl::QuadTo(a, b) => new_outer.quad_to(a, b),
                    PathEl::CurveTo(a, b, c) => new_outer.curve_to(a, b, c),
                    PathEl::ClosePath => new_outer.close_path(),
                }
            }
        }

        self.outer_boundary = new_outer;

        // Re-run the interior knob collision pass with the boundary
        // knobs as fixed obstacles. Interior knobs near the
        // silhouette flip / slide / shrink to clear the boundary
        // bumps; the boundary knobs themselves stay put.
        self.resolve_knob_collisions(connector, clearance_mm, &boundary_obbs);
    }

    /// Compute the pre-resolution average knob "base" — the mean of
    /// `min(length, cross_length)` across every interior edge with a
    /// connector — and cap each edge's `cross_length` to that
    /// average. Stores the cap on `self.knob_base_cap` so other
    /// passes (`knob_outer_boundary`) can use the same ceiling.
    ///
    /// Why: knob size = `0.25 × min(length, cross_length)`, so an
    /// edge with unusually-far-apart adjacent piece centers (a "fat"
    /// neighborhood) produces a noticeably larger knob than its
    /// peers. Capping `cross_length` at the average pulls the
    /// outliers down without affecting edges that are already at or
    /// below average. The cap is computed *before* any resolution
    /// adjustment so the user-visible cap reflects the natural
    /// puzzle-piece scale.
    ///
    /// Should be called *after* `generate_connectors` and *before*
    /// `resolve_knob_collisions`.
    pub fn compute_and_cap_avg_knob_base(
        &mut self,
        connector: &dyn crate::connector::ConnectorGenerator,
    ) {
        // Gather pre-cap base values across knobbed edges.
        let mut total = 0.0;
        let mut count = 0usize;
        for edge in &self.edges {
            let Some(params) = edge.connector_params.as_ref() else {
                continue;
            };
            total += params.length.min(params.cross_length) * params.knob_scale;
            count += 1;
        }
        if count == 0 {
            return;
        }
        let avg = total / count as f64;
        self.knob_base_cap = avg;

        // Apply the cap to every connector whose `cross_length`
        // exceeds it. Length stays canonical; only `cross_length` is
        // pulled down so `min(length, cross_length)` lands at `avg`
        // (or stays smaller for edges already below average).
        let mut rng = crate::seed::create_rng("knob-cap");
        for edge in &mut self.edges {
            let Some(params) = edge.connector_params.as_mut() else {
                continue;
            };
            let cap = avg / params.knob_scale;
            if params.cross_length > cap {
                params.cross_length = cap;
                let curves = connector.generate(params, &mut rng);
                edge.connector = Some(curves);
            }
        }
    }

    pub fn merge_short_edges(&mut self, threshold: f64) {
        let to_suppress: Vec<usize> = self
            .edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| (e.length() < threshold).then_some(i))
            .collect();
        self.merge_across_edges(&to_suppress);
    }

    /// Merge fragile pieces into their longest-shared-edge neighbor.
    /// Four flag conditions, any of which triggers a merge:
    ///
    /// 1. **Acute corner under `min_angle_deg`** AND area below
    ///    `AREA_SLIVER_FRACTION × median`. Catches sharp-tip slivers
    ///    where a whimsy/border pointed feature (heart bottom, star
    ///    tip) gets inherited by a small CVT cell.
    /// 2. **Total area below `TINY_PIECE_AREA_FRACTION × median`**
    ///    regardless of corner shape. Catches the rare smooth-blob
    ///    sliver that forms when a curved whimsy contour clips a
    ///    Voronoi cell down to a tiny rounded region without producing
    ///    any acute corner.
    /// 3. **Compactness below `WEDGY_COMPACTNESS_THRESHOLD`**. Catches
    ///    normal-area cells with a thin tendril — typically when two
    ///    seeds end up close-and-parallel to a whimsy contour and one
    ///    cell gets squeezed into a long sliver between bisector and
    ///    contour. The cell's total area is normal but `4π·area/perim²`
    ///    drops well below typical CVT values.
    /// 4. **Smallest subpath area below `TINY_PIECE_AREA_FRACTION ×
    ///    median`**. Catches the case where a whimsy splits a Voronoi
    ///    cell into disjoint regions: the cell's *total* area is
    ///    normal (main body + fragment), but the small fragment
    ///    renders as a visibly-tiny piece even though no seed sits
    ///    inside it.
    ///
    /// Should be called *before* `merge_short_edges` and
    /// `generate_connectors`: sliver-merges can produce new short
    /// edges, and anything generating connectors needs a stable edge
    /// set.
    pub fn merge_thin_pieces(&mut self, min_angle_deg: f64) {
        let min_angle_rad = min_angle_deg.to_radians();

        // Per-piece metrics: a normal CVT cell with an acute vertex
        // (an obtuse Delaunay triangle's circumcenter makes for an
        // acute Voronoi corner) is still a large, structurally-sound
        // piece — only truly small pieces with acute corners are
        // fragile. Perimeter feeds the wedgy-cell check (tendril
        // cells have normal area but huge perimeter). Min-subpath
        // area catches whimsy-clipped cells whose outline is two
        // disjoint regions (one normal blob + one tiny fragment).
        let metrics: Vec<PolyMetrics> = self
            .pieces
            .iter()
            .map(|p| {
                p.outline.as_ref().map(polygon_metrics).unwrap_or(
                    PolyMetrics {
                        area: 0.0,
                        perimeter: 0.0,
                        min_subpath_area: 0.0,
                    },
                )
            })
            .collect();
        let areas: Vec<f64> = metrics.iter().map(|m| m.area).collect();
        let mut sorted_areas: Vec<f64> = areas
            .iter()
            .copied()
            .filter(|a| *a > 0.0)
            .collect();
        sorted_areas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let Some(&median_area) = sorted_areas.get(sorted_areas.len() / 2) else {
            return;
        };
        if median_area <= 0.0 {
            return;
        }
        let area_threshold = median_area * AREA_SLIVER_FRACTION;
        let tiny_threshold = median_area * TINY_PIECE_AREA_FRACTION;

        let mut to_suppress: Vec<usize> = Vec::new();
        for (idx, piece) in self.pieces.iter().enumerate() {
            let Some(outline) = piece.outline.as_ref() else {
                continue;
            };
            // Four ways to be a sliver candidate:
            // 1. Below the angle-based size gate AND has an acute
            //    corner (sharp-tip pieces from heart/star whimsies).
            // 2. Below the absolute-area floor regardless of shape
            //    (smooth-blob pieces from curved whimsy boundaries).
            // 3. Compactness below the wedgy threshold — normal-area
            //    cells with a thin tendril running parallel to a
            //    whimsy contour.
            // 4. Smallest disjoint subpath below the tiny-area floor —
            //    a whimsy clip splitting a cell into normal-blob plus
            //    tiny-fragment renders the fragment as a visibly small
            //    piece despite normal total area.
            let m = &metrics[idx];
            let acute_candidate = m.area < area_threshold
                && polygon_has_acute_corner(outline, min_angle_rad);
            // No `> 0.0` guard — a degenerate/zero-area outline is
            // *more* of a sliver, not less, and we want it merged
            // away regardless.
            let tiny_candidate = m.area < tiny_threshold;
            let compactness = if m.perimeter > 0.0 {
                4.0 * std::f64::consts::PI * m.area / (m.perimeter * m.perimeter)
            } else {
                0.0
            };
            let wedgy_candidate =
                m.area > 0.0 && compactness < WEDGY_COMPACTNESS_THRESHOLD;
            let split_candidate = m.min_subpath_area > 0.0
                && m.min_subpath_area < tiny_threshold
                && m.min_subpath_area < m.area; // i.e. there are 2+ subpaths
            if !acute_candidate
                && !tiny_candidate
                && !wedgy_candidate
                && !split_candidate
            {
                continue;
            }
            // Merge across the longest internal edge — the widest
            // interface with a neighbor produces the cleanest merge.
            let Some(&longest) = piece.edge_indices.iter().max_by(|&&a, &&b| {
                self.edges[a]
                    .length()
                    .partial_cmp(&self.edges[b].length())
                    .unwrap()
            }) else {
                continue;
            };
            to_suppress.push(longest);
        }
        to_suppress.sort_unstable();
        to_suppress.dedup();
        self.merge_across_edges(&to_suppress);
    }

    /// Laplacian smoothing on the shared edge mesh to flatten the
    /// edge-length distribution (and therefore the knob-size
    /// distribution, since knob size is proportional to edge length).
    ///
    /// Builds a vertex list from every edge endpoint (deduping by
    /// position), then iterates: each interior vertex moves to the
    /// centroid of its neighbors along incident edges.
    ///
    /// Pinned (never-moved) vertices:
    /// - **Boundary-touching**: vertices whose degree (incident edge
    ///   count) is less than 3. In a pure Voronoi, interior vertices
    ///   always have degree ≥ 3 where cells meet; degree 1 or 2
    ///   indicates the vertex is clipped against the outer/whimsy
    ///   boundary and moving it would desync the edge endpoints from
    ///   the rendered `outer_boundary`.
    /// - **Anchor-protected**: vertices within `pin_radius` mm of any
    ///   point in `pinned_centers`. Pass the anchor-seed positions
    ///   from `CvtParams::anchors` here so the sharp-corner bisector
    ///   geometry stays locked.
    ///
    /// This breaks the "edges are perpendicular bisectors" Voronoi
    /// property — consumers that care about strict Voronoi semantics
    /// should run their analyses before calling this. For our SVG /
    /// binary export, only the polyline mesh matters.
    ///
    /// Piece outlines are set to `None` (they were derived from the
    /// pre-smoothing cell polygons); downstream code that needs them
    /// should re-derive from edges + outer boundary.
    pub fn smooth_edges(
        &mut self,
        iterations: usize,
        pinned_centers: &[Point],
        pin_radius: f64,
    ) {
        if iterations == 0 || self.edges.is_empty() {
            return;
        }

        // Dedup vertices: two endpoints within `TOL` of each other are
        // treated as the same vertex. Voronoi vertices are exact, so
        // tolerance really just absorbs floating-point noise.
        //
        // O(1) amortized via a spatial hash. Cell size = 2·TOL so any
        // pair of points within TOL falls in the same cell or one of
        // the eight neighbors; per-query scan checks the 3×3 cell
        // window. The naive O(n²) linear scan we used to do here was
        // ~225M comparisons at 5k pieces.
        const TOL: f64 = 1e-6;
        const CELL: f64 = TOL * 2.0;
        let mut verts: Vec<Point> = Vec::new();
        let mut edge_endpoints: Vec<(usize, usize)> =
            Vec::with_capacity(self.edges.len());
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let cell_of = |p: Point| -> (i64, i64) {
            ((p.x / CELL).floor() as i64, (p.y / CELL).floor() as i64)
        };

        let find_or_add = |p: Point,
                           verts: &mut Vec<Point>,
                           grid: &mut HashMap<(i64, i64), Vec<usize>>|
         -> usize {
            let (cx, cy) = cell_of(p);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Some(bucket) = grid.get(&(cx + dx, cy + dy)) {
                        for &i in bucket {
                            if (verts[i].x - p.x).abs() < TOL
                                && (verts[i].y - p.y).abs() < TOL
                            {
                                return i;
                            }
                        }
                    }
                }
            }
            let idx = verts.len();
            verts.push(p);
            grid.entry((cx, cy)).or_default().push(idx);
            idx
        };

        for edge in &self.edges {
            let a = find_or_add(edge.start, &mut verts, &mut grid);
            let b = find_or_add(edge.end, &mut verts, &mut grid);
            edge_endpoints.push((a, b));
        }

        // Degree (incident edge count) per vertex, and neighbor list.
        let n = verts.len();
        let mut degree = vec![0usize; n];
        let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(a, b) in &edge_endpoints {
            degree[a] += 1;
            degree[b] += 1;
            neighbors[a].push(b);
            neighbors[b].push(a);
        }

        // Pinned flags: boundary-touching OR near an anchor center.
        let pin_radius_sq = pin_radius * pin_radius;
        let pinned: Vec<bool> = (0..n)
            .map(|i| {
                if degree[i] < 3 {
                    return true;
                }
                for c in pinned_centers {
                    let dx = verts[i].x - c.x;
                    let dy = verts[i].y - c.y;
                    if dx * dx + dy * dy <= pin_radius_sq {
                        return true;
                    }
                }
                false
            })
            .collect();

        // Jacobi-style smoothing: read from `verts`, write to `next`,
        // swap. Averaging neighbors equalizes incident edge lengths.
        let mut next = verts.clone();
        for _ in 0..iterations {
            for i in 0..n {
                if pinned[i] || neighbors[i].is_empty() {
                    next[i] = verts[i];
                    continue;
                }
                let mut sx = 0.0;
                let mut sy = 0.0;
                for &j in &neighbors[i] {
                    sx += verts[j].x;
                    sy += verts[j].y;
                }
                let k = neighbors[i].len() as f64;
                next[i] = Point::new(sx / k, sy / k);
            }
            std::mem::swap(&mut verts, &mut next);
        }

        // Write the smoothed vertex positions back into every edge.
        for (edge, &(a, b)) in self.edges.iter_mut().zip(&edge_endpoints) {
            edge.start = verts[a];
            edge.end = verts[b];
        }

        // Piece outlines came from the pre-smoothing Voronoi clip and
        // are now stale — drop them so downstream consumers derive
        // piece shapes from edges + outer_boundary on demand.
        for p in &mut self.pieces {
            p.outline = None;
        }
    }

    /// Shared merge machinery: given a list of edge indices to collapse,
    /// run union-find over the pieces they connect, renumber pieces,
    /// rebuild the edge list, and wipe per-piece outlines (which can't
    /// be trivially unioned and aren't consumed by any downstream
    /// export step anyway).
    fn merge_across_edges(&mut self, to_suppress: &[usize]) {
        if to_suppress.is_empty() {
            return;
        }

        let mut uf = UnionFind::new(self.pieces.len());
        for &ei in to_suppress {
            let (a, b) = self.edges[ei].pieces;
            uf.union(a, b);
        }

        let mut old_to_new = vec![usize::MAX; self.pieces.len()];
        let mut next_new_id = 0usize;
        for old_id in 0..self.pieces.len() {
            let root = uf.find(old_id);
            if old_to_new[root] == usize::MAX {
                old_to_new[root] = next_new_id;
                next_new_id += 1;
            }
        }
        let new_piece_of: Vec<usize> = (0..self.pieces.len())
            .map(|i| old_to_new[uf.find(i)])
            .collect();

        let mut new_pieces: Vec<LayoutPiece> = (0..next_new_id)
            .map(|i| LayoutPiece {
                id: i,
                center: Point::ZERO,
                edge_indices: Vec::new(),
                outline: None,
            })
            .collect();
        let mut counts = vec![0u32; next_new_id];
        for (old_id, piece) in self.pieces.iter().enumerate() {
            let new_id = new_piece_of[old_id];
            let c = &mut new_pieces[new_id].center;
            c.x += piece.center.x;
            c.y += piece.center.y;
            counts[new_id] += 1;
        }
        for (i, p) in new_pieces.iter_mut().enumerate() {
            let n = counts[i] as f64;
            if n > 0.0 {
                p.center = Point::new(p.center.x / n, p.center.y / n);
            }
        }

        let suppress_set: std::collections::HashSet<usize> =
            to_suppress.iter().copied().collect();
        let mut new_edges: Vec<LayoutEdge> = Vec::with_capacity(self.edges.len());
        for (old_idx, edge) in self.edges.iter().enumerate() {
            if suppress_set.contains(&old_idx) {
                continue;
            }
            let na = new_piece_of[edge.pieces.0];
            let nb = new_piece_of[edge.pieces.1];
            if na == nb {
                continue;
            }
            let new_idx = new_edges.len();
            new_edges.push(LayoutEdge {
                start: edge.start,
                end: edge.end,
                direction: edge.direction,
                connector: edge.connector.clone(),
                connector_params: edge.connector_params.clone(),
                pieces: (na, nb),
            });
            new_pieces[na].edge_indices.push(new_idx);
            new_pieces[nb].edge_indices.push(new_idx);
        }

        self.edges = new_edges;
        self.pieces = new_pieces;
    }
}

/// Return `true` if any vertex of `path` (after flattening) has an
/// interior angle sharper than `threshold_rad`.
///
/// Vertex types this fires for, assuming the caller already filtered
/// by piece size:
/// - Flattened-curve cusps (heart tip, star point).
/// - Voronoi vertices where two bisectors converge acutely.
/// - "Bisector meets curve" corners where a CVT edge approaches a
///   whimsy contour at a shallow angle.
///
/// The size filter (small area ⇒ fragile) is what keeps this from
/// chewing on normal cells that happen to have one acute corner at
/// the join of two long bisector edges — those are large pieces that
/// happen to be pointy, not structurally-fragile slivers.
/// Build a list of "wall" OBBs along every inner subpath of `path`
/// (whimsy holes and similar interior contours). The outer subpath
/// is skipped; only inner ones contribute. Each OBB is a degenerate-
/// thin rectangle hugging one polyline segment of the flattened
/// contour — knob OBBs (which carry their own clearance padding)
/// that overlap one of these are within clearance of the contour.
///
/// Flatten tolerance is 0.5 mm — fine enough to follow heart and
/// star contours faithfully, coarse enough to keep the obstacle
/// count manageable.
fn boundary_hole_obstacles(path: &BezPath) -> Vec<KnobObb> {
    /// Tiny perpendicular thickness used so the obstacle isn't a
    /// degenerate-zero-area rectangle (which would break SAT axis
    /// derivation). 0.01 mm is well below visible scale and well
    /// below any meaningful clearance value.
    const SEGMENT_OBSTACLE_THICKNESS: f64 = 0.01;

    let subpaths = split_subpaths(path);
    if subpaths.len() < 2 {
        return Vec::new();
    }
    let mut obstacles: Vec<KnobObb> = Vec::new();
    for sub in &subpaths[1..] {
        // Flatten each inner subpath to a polyline.
        let mut points: Vec<Point> = Vec::new();
        kurbo::flatten(sub.iter(), 0.5, |el| match el {
            PathEl::MoveTo(p) | PathEl::LineTo(p) => points.push(p),
            _ => {}
        });
        let n = points.len();
        if n < 2 {
            continue;
        }
        // Dedupe trailing vertex if it coincides with the start
        // (closed-path artifact from `kurbo::flatten`).
        let last = points.last().copied().unwrap();
        let first = points[0];
        let polyline_n = if n >= 2 && (last - first).hypot() < 1e-6 {
            n - 1
        } else {
            n
        };
        for i in 0..polyline_n {
            let a = points[i];
            let b = points[(i + 1) % polyline_n];
            let length = (b - a).hypot();
            if length < 1e-9 {
                continue;
            }
            // Obstacle in segment-local frame: thin rectangle along x.
            let local: [Point; 4] = [
                Point::new(0.0, -SEGMENT_OBSTACLE_THICKNESS),
                Point::new(length, -SEGMENT_OBSTACLE_THICKNESS),
                Point::new(length, SEGMENT_OBSTACLE_THICKNESS),
                Point::new(0.0, SEGMENT_OBSTACLE_THICKNESS),
            ];
            let aff = edge_transform(a, b);
            obstacles.push(KnobObb {
                corners: local.map(|p| aff * p),
            });
        }
    }
    obstacles
}

/// Outgoing-tangent vector at the END of a path segment (`t = 1`).
/// Direction only — magnitude is meaningless to callers.
fn segment_end_tangent(seg: &PathSeg) -> Vec2 {
    match *seg {
        PathSeg::Line(l) => Vec2::new(l.p1.x - l.p0.x, l.p1.y - l.p0.y),
        PathSeg::Quad(q) => Vec2::new(q.p2.x - q.p1.x, q.p2.y - q.p1.y),
        PathSeg::Cubic(c) => Vec2::new(c.p3.x - c.p2.x, c.p3.y - c.p2.y),
    }
}

/// Incoming-tangent vector at the START of a path segment (`t = 0`).
fn segment_start_tangent(seg: &PathSeg) -> Vec2 {
    match *seg {
        PathSeg::Line(l) => Vec2::new(l.p1.x - l.p0.x, l.p1.y - l.p0.y),
        PathSeg::Quad(q) => Vec2::new(q.p1.x - q.p0.x, q.p1.y - q.p0.y),
        PathSeg::Cubic(c) => Vec2::new(c.p1.x - c.p0.x, c.p1.y - c.p0.y),
    }
}

/// Start point of a path segment.
fn segment_start_point(seg: &PathSeg) -> Point {
    match *seg {
        PathSeg::Line(l) => l.p0,
        PathSeg::Quad(q) => q.p0,
        PathSeg::Cubic(c) => c.p0,
    }
}

/// Split a `BezPath` into its constituent subpaths. Each `MoveTo`
/// starts a new subpath; subsequent commands append to the current
/// one until the next `MoveTo` (or end of input). Used by
/// `knob_outer_boundary` to identify the outer ring vs. inner whimsy
/// holes and by other passes that need to walk one closed contour at
/// a time.
fn split_subpaths(path: &BezPath) -> Vec<BezPath> {
    let mut subpaths: Vec<BezPath> = Vec::new();
    let mut current = BezPath::new();
    let mut started = false;
    for el in path.iter() {
        match el {
            PathEl::MoveTo(p) => {
                if started {
                    subpaths.push(std::mem::take(&mut current));
                }
                current.move_to(p);
                started = true;
            }
            PathEl::LineTo(p) => current.line_to(p),
            PathEl::QuadTo(a, b) => current.quad_to(a, b),
            PathEl::CurveTo(a, b, c) => current.curve_to(a, b, c),
            PathEl::ClosePath => current.close_path(),
        }
    }
    if started {
        subpaths.push(current);
    }
    subpaths
}

fn polygon_has_acute_corner(path: &BezPath, threshold_rad: f64) -> bool {
    // Build one subpath per MoveTo. Drop the last vertex of a
    // closed-path subpath if it coincides with the first (kurbo
    // emits the endpoint-of-last-curve at the MoveTo origin for
    // closed paths, producing a duplicate that would otherwise hide
    // the cusp at that vertex behind a zero-length neighbor).
    let mut subpaths: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    kurbo::flatten(path.iter(), 0.25, |el| match el {
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
        for i in 0..n {
            let prev = sub[(i + n - 1) % n];
            let curr = sub[i];
            let next = sub[(i + 1) % n];
            let u = prev - curr;
            let v = next - curr;
            let lu = u.hypot();
            let lv = v.hypot();
            if lu < 1e-6 || lv < 1e-6 {
                continue;
            }
            let cos = ((u.x * v.x + u.y * v.y) / (lu * lv)).clamp(-1.0, 1.0);
            let angle = cos.acos();
            if angle < threshold_rad {
                return true;
            }
        }
    }
    false
}

/// Geometric metrics of a closed path, computed from a single flatten
/// pass.
///
/// - `area`: total area via the shoelace formula across every closed
///   subpath (holes subtract). Absolute value covers CW vs CCW winding.
/// - `perimeter`: sum of polyline-segment lengths across every
///   subpath, holes included.
/// - `min_subpath_area`: the smallest non-zero per-subpath area. A
///   single-blob cell has just one subpath (so `min == area`); a cell
///   split into disjoint regions by whimsy clipping reports the
///   smallest fragment, which is the "secretly tiny piece" signal that
///   total area alone misses.
struct PolyMetrics {
    area: f64,
    perimeter: f64,
    min_subpath_area: f64,
}

fn polygon_metrics(path: &BezPath) -> PolyMetrics {
    let mut subpaths: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    kurbo::flatten(path.iter(), 0.25, |el| match el {
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

    let mut signed_total = 0.0;
    let mut perim = 0.0;
    let mut min_sub = f64::INFINITY;
    for sub in &subpaths {
        let n = sub.len();
        if n < 3 {
            continue;
        }
        let mut signed_sub = 0.0;
        for i in 0..n {
            let j = (i + 1) % n;
            signed_sub += sub[i].x * sub[j].y - sub[j].x * sub[i].y;
            perim += (sub[j] - sub[i]).hypot();
        }
        signed_total += signed_sub;
        let sub_area = (signed_sub * 0.5).abs();
        if sub_area > 0.0 && sub_area < min_sub {
            min_sub = sub_area;
        }
    }
    let min_subpath_area = if min_sub.is_finite() { min_sub } else { 0.0 };
    PolyMetrics {
        area: (signed_total * 0.5).abs(),
        perimeter: perim,
        min_subpath_area,
    }
}

/// Lightweight union-find over piece indices. Used by
/// [`PuzzleLayout::merge_short_edges`] to batch edge suppressions that
/// might form chains.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, i: usize) -> usize {
        let mut r = i;
        while self.parent[r] != r {
            r = self.parent[r];
        }
        let mut j = i;
        while self.parent[j] != r {
            let next = self.parent[j];
            self.parent[j] = r;
            j = next;
        }
        r
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// A piece counts as a "sliver candidate" for the acute-corner merge
/// when its polygon area is smaller than this fraction of the median
/// piece area. Keeps merge pressure off full-size cells that happen
/// to have one acute Voronoi corner.
const AREA_SLIVER_FRACTION: f64 = 0.5;

/// Absolute area floor for the sliver merge: any piece smaller than
/// this fraction of the median area is merged regardless of corner
/// shape. Catches the rare CVT edge case where a smooth-cornered
/// blob piece forms next to a curved whimsy contour and slips past
/// the angle-based detector. Set generously enough to absorb any
/// piece that's visibly half-size or smaller than its peers.
const TINY_PIECE_AREA_FRACTION: f64 = 0.25;

/// Compactness threshold (`4π·area / perimeter²`) below which a piece
/// is considered "wedgy" — high perimeter relative to area, which is
/// the signature of a normal-area cell with a thin tendril/protrusion
/// (the bisector between two seeds running close-and-parallel to a
/// whimsy contour squeezes one cell into a tendril). A circle scores
/// 1.0; an equilateral triangle ~0.6; a square ~0.785; typical CVT
/// cells land 0.6–0.9. Below 0.4 you're looking at a cell with a
/// visible tendril attached to its main body.
const WEDGY_COMPACTNESS_THRESHOLD: f64 = 0.4;

/// Regenerate a knob from `original` with overrides for direction,
/// scale (applied to `cross_length`), and offset along the edge.
/// Updates `edge.connector`, `edge.connector_params`, and
/// `edge.direction` in place. Used by every step of
/// `resolve_knob_collisions` (flip, slide, shrink) so the rebuild
/// path is consistent.
fn apply_knob_variant(
    edge: &mut LayoutEdge,
    original: &EdgeParams,
    direction: TabDirection,
    scale: f64,
    offset: f64,
    connector: &dyn crate::connector::ConnectorGenerator,
) {
    let variant = EdgeParams {
        length: original.length,
        cross_length: original.cross_length * scale,
        direction,
        offset,
        knob_scale: original.knob_scale,
    };
    let mut rng = crate::seed::create_rng("knob-resolve");
    let curves = connector.generate(&variant, &mut rng);
    edge.direction = direction;
    edge.connector = Some(curves);
    edge.connector_params = Some(variant);
}

/// Oriented bounding box of a knob in global mm coords. The OBB sits
/// in the edge-local frame as a rectangle and is rotated/translated
/// to global via `edge_transform`. We store its 4 corner points
/// explicitly; the SAT check below derives axes from those corners.
#[derive(Debug, Clone, Copy)]
pub struct KnobObb {
    corners: [Point; 4],
}

/// Build a knob's OBB. The knob occupies edge-local
/// `x ∈ [center_x - knob_w, center_x + knob_w]` and
/// `y ∈ [0, knob_h]` (Out) or `[-knob_h, 0]` (In), where:
/// - `center_x = length/2 + offset`
/// - `knob_w   = base × KNOB_WIDTH_RATIO`
/// - `knob_h   = base × KNOB_WIDTH_RATIO × KNOB_HEIGHT_RATIO`
/// - `base     = min(length, cross_length) × knob_scale`
///
/// We add a small fudge to the perpendicular extent for the bezier
/// top overshoot (peak slightly above `knob_h`) plus `clearance_mm` on
/// every side as the structural gap caller wants enforced. Returns
/// `None` if the edge has no connector or no `connector_params`.
fn knob_global_obb(edge: &LayoutEdge, clearance_mm: f64) -> Option<KnobObb> {
    edge.connector.as_ref()?;
    let params = edge.connector_params.as_ref()?;
    Some(obb_from_chord(edge.start, edge.end, params, clearance_mm))
}

/// Build a `KnobObb` from a chord (`start` → `end`) and connector
/// `params`, inflated by `clearance_mm` on every side. Used by
/// `knob_global_obb` for `LayoutEdge` knobs and by
/// `knob_outer_boundary` for boundary knobs (which aren't backed by a
/// `LayoutEdge` but otherwise have identical OBB geometry).
fn obb_from_chord(start: Point, end: Point, params: &EdgeParams, clearance_mm: f64) -> KnobObb {
    // Match `classic_connector::ClassicKnobConnector::generate`.
    const KNOB_W_FACTOR: f64 = 0.25;
    const KNOB_H_FACTOR: f64 = 0.30; // KNOB_W_FACTOR × KNOB_HEIGHT_RATIO (=1.2)
    const TOP_OVERSHOOT: f64 = 1.05;

    let base = params.length.min(params.cross_length) * params.knob_scale;
    let knob_w = base * KNOB_W_FACTOR + clearance_mm;
    let knob_h = base * KNOB_H_FACTOR * TOP_OVERSHOOT + clearance_mm;
    let center_x = params.length * 0.5 + params.offset;

    let dir_sign = match params.direction {
        TabDirection::Out => 1.0,
        TabDirection::In => -1.0,
    };
    let y_lo = if dir_sign > 0.0 { -clearance_mm } else { -knob_h };
    let y_hi = if dir_sign > 0.0 { knob_h } else { clearance_mm };

    let local: [Point; 4] = [
        Point::new(center_x - knob_w, y_lo),
        Point::new(center_x + knob_w, y_lo),
        Point::new(center_x + knob_w, y_hi),
        Point::new(center_x - knob_w, y_hi),
    ];
    let transform = edge_transform(start, end);
    KnobObb {
        corners: local.map(|p| transform * p),
    }
}

/// SAT penetration depth between two OBBs. Returns the smallest
/// projection-overlap across the 4 candidate separating axes — i.e.
/// the depth by which the rectangles overlap, in mm. Returns `0.0`
/// when the rectangles are separated. Higher value = deeper overlap.
fn obb_penetration(a: &KnobObb, b: &KnobObb) -> f64 {
    let project = |corners: &[Point; 4], axis: Vec2| -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for p in corners {
            let pp = p.x * axis.x + p.y * axis.y;
            if pp < lo {
                lo = pp;
            }
            if pp > hi {
                hi = pp;
            }
        }
        (lo, hi)
    };
    let normalize = |v: Vec2| -> Vec2 {
        let len = v.hypot();
        if len < 1e-9 {
            Vec2::new(1.0, 0.0)
        } else {
            Vec2::new(v.x / len, v.y / len)
        }
    };
    let edge_axis = |c: &[Point; 4], i: usize| -> Vec2 {
        let p0 = c[i];
        let p1 = c[(i + 1) % 4];
        normalize(Vec2::new(p1.x - p0.x, p1.y - p0.y))
    };
    let axes = [
        edge_axis(&a.corners, 0),
        edge_axis(&a.corners, 1),
        edge_axis(&b.corners, 0),
        edge_axis(&b.corners, 1),
    ];
    let mut min_overlap = f64::INFINITY;
    for axis in axes {
        let (a_lo, a_hi) = project(&a.corners, axis);
        let (b_lo, b_hi) = project(&b.corners, axis);
        let overlap = a_hi.min(b_hi) - a_lo.max(b_lo);
        if overlap < min_overlap {
            min_overlap = overlap;
        }
    }
    min_overlap.max(0.0)
}

/// Separating-axis theorem (SAT) overlap test for two OBBs given as
/// 4-corner rectangles. Returns `true` if the rectangles overlap (or
/// touch). Tests four candidate separating axes — the two edge
/// directions of each rectangle. If projections onto every axis
/// overlap, the rectangles overlap.
fn obbs_overlap(a: &KnobObb, b: &KnobObb) -> bool {
    let project = |corners: &[Point; 4], axis: Vec2| -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for p in corners {
            let proj = p.x * axis.x + p.y * axis.y;
            if proj < lo {
                lo = proj;
            }
            if proj > hi {
                hi = proj;
            }
        }
        (lo, hi)
    };
    let edge_axis = |c: &[Point; 4], i: usize| -> Vec2 {
        let p0 = c[i];
        let p1 = c[(i + 1) % 4];
        Vec2::new(p1.x - p0.x, p1.y - p0.y)
    };
    let axes = [
        edge_axis(&a.corners, 0),
        edge_axis(&a.corners, 1),
        edge_axis(&b.corners, 0),
        edge_axis(&b.corners, 1),
    ];
    for axis in axes {
        let (a_lo, a_hi) = project(&a.corners, axis);
        let (b_lo, b_hi) = project(&b.corners, axis);
        if a_hi < b_lo || b_hi < a_lo {
            return false; // separating axis found
        }
    }
    true
}

// ─── Knob safety probe ───────────────────────────────────────────────

/// Sample the classic knob silhouette on both perpendicular sides of
/// the edge and report whether each side stays inside the (flattened)
/// boundary polygon.
///
/// The classic knob (see `classic_connector.rs`) has half-width
/// `knob_w = 0.25 × base` and peak depth `knob_h = 0.30 × base`, where
/// `base = min(length, cross_length)`. We sample three points per side
/// — the center tip plus two shoulder points — because a tip-only
/// probe can land in a safe region while the shoulders clip a nearby
/// whimsy corner or concavity.
///
/// `relaxed` controls how strict the "safe" verdict is:
/// - `false` (canonical): side is safe only when ALL 3 samples are
///   inside the boundary. Used by committed/exported builds so the
///   resulting SVG never has a knob silhouette crossing the cut line.
/// - `true` (drag preview): side is safe when ANY 1+ sample is inside.
///   Marginal cases — where a moving whimsy is mid-way through the
///   probe samples — keep their hash-default direction instead of
///   spuriously flipping every frame. Knobs may visually overlap the
///   whimsy boundary mid-drag, which is a transient rendering blip
///   that resolves on commit.
///
/// `boundary_polygon` + `boundary_index` are the pre-flattened
/// boundary plus its Y-bucket spatial index. Both are reused across
/// the O(edges) calls in `generate_connectors` /
/// `resolve_knob_collisions` so we don't rebuild the index per probe.
fn knob_safety_probe_with_style(
    a: Point,
    b: Point,
    cross_length: f64,
    scale: f64,
    offset: f64,
    boundary_polygon: &[Vec<Point>],
    boundary_index: &crate::flat_boundary::BoundaryIndex,
) -> (bool, bool) {
    const KNOB_W_FACTOR: f64 = 0.25;
    const KNOB_H_FACTOR: f64 = 0.30;
    const PROBE_OVERSHOOT: f64 = 1.15;
    const SHOULDER_W: f64 = 0.7;
    const SHOULDER_H: f64 = 0.85;

    let length = (b - a).hypot();
    if length < 1e-9 {
        return (true, true);
    }
    let base = length.min(cross_length) * scale;
    let knob_w = base * KNOB_W_FACTOR;
    let knob_h = base * KNOB_H_FACTOR;

    let dx = (b.x - a.x) / length;
    let dy = (b.y - a.y) / length;
    // Edge-local +y in global coords is (-dy, dx) — the "Out" side.
    let px = -dy;
    let py = dx;
    let mx = (a.x + b.x) * 0.5 + dx * offset;
    let my = (a.y + b.y) * 0.5 + dy * offset;

    let samples: [(f64, f64); 3] = [
        (0.0, knob_h * PROBE_OVERSHOOT),
        (-knob_w * SHOULDER_W, knob_h * SHOULDER_H),
        (knob_w * SHOULDER_W, knob_h * SHOULDER_H),
    ];
    let side_safe = |sign: f64| -> bool {
        samples.iter().all(|(t, n)| {
            let offset_n = n * sign;
            let p = Point::new(
                mx + dx * t + px * offset_n,
                my + dy * t + py * offset_n,
            );
            crate::flat_boundary::polygon_contains_indexed(
                boundary_polygon,
                boundary_index,
                p,
            )
        })
    };
    (side_safe(1.0), side_safe(-1.0))
}

// ─── Export functions ────────────────────────────────────────────────

/// Generate a complete SVG from a `PuzzleLayout`. The outer boundary
/// becomes a closed subpath; each internal edge contributes one open
/// subpath — curved if the edge has a connector, straight line if it
/// doesn't (edges too short to host a sensible knob leave
/// `connector = None`).
pub fn layout_generate_svg(layout: &PuzzleLayout) -> String {
    let mut combined = BezPath::new();

    for el in layout.outer_boundary.iter() {
        match el {
            PathEl::MoveTo(p) => combined.move_to(p),
            PathEl::LineTo(p) => combined.line_to(p),
            PathEl::CurveTo(p1, p2, p3) => combined.curve_to(p1, p2, p3),
            PathEl::ClosePath => combined.close_path(),
            _ => {}
        }
    }

    for edge in &layout.edges {
        match edge.connector.as_ref() {
            Some(curves) => {
                let transform = edge_transform(edge.start, edge.end);
                let first_p0 = transform * curves[0].p0;
                combined.move_to(first_p0);
                for curve in curves {
                    let p1 = transform * curve.p1;
                    let p2 = transform * curve.p2;
                    let p3 = transform * curve.p3;
                    combined.curve_to(p1, p2, p3);
                }
            }
            None => {
                // No knob — straight line from edge start to end.
                combined.move_to(edge.start);
                combined.line_to(edge.end);
            }
        }
    }

    let path_data = combined.to_svg();
    build_svg_document(&path_data, layout.width, layout.height)
}

/// Serialize a layout's internal edges as a command-prefixed flat
/// `f64` array. Edges with a connector emit `CMD_MOVE_TO` + a run of
/// `CMD_CURVE_TO`s; edges without one (short edges that opted out of a
/// knob) emit `CMD_MOVE_TO` + `CMD_LINE_TO`.
pub fn layout_edges_to_binary(layout: &PuzzleLayout) -> Vec<f64> {
    let mut data: Vec<f64> = Vec::new();
    for edge in &layout.edges {
        match edge.connector.as_ref() {
            Some(curves) => {
                let transform = edge_transform(edge.start, edge.end);
                let first_p0 = transform * curves[0].p0;
                data.push(CMD_MOVE_TO);
                data.push(first_p0.x);
                data.push(first_p0.y);
                for curve in curves {
                    let p1 = transform * curve.p1;
                    let p2 = transform * curve.p2;
                    let p3 = transform * curve.p3;
                    data.push(CMD_CURVE_TO);
                    data.push(p1.x);
                    data.push(p1.y);
                    data.push(p2.x);
                    data.push(p2.y);
                    data.push(p3.x);
                    data.push(p3.y);
                }
            }
            None => {
                data.push(CMD_MOVE_TO);
                data.push(edge.start.x);
                data.push(edge.start.y);
                data.push(CMD_LINE_TO);
                data.push(edge.end.x);
                data.push(edge.end.y);
            }
        }
    }
    data
}

/// Serialize a layout's outer boundary as a command-prefixed flat `f64`
/// array. Supports MoveTo / LineTo / CurveTo / ClosePath — matches the
/// border format used everywhere else.
pub fn layout_border_to_binary(layout: &PuzzleLayout) -> Vec<f64> {
    let mut data = Vec::with_capacity(128);
    for el in layout.outer_boundary.iter() {
        match el {
            PathEl::MoveTo(p) => {
                data.push(CMD_MOVE_TO);
                data.push(p.x);
                data.push(p.y);
            }
            PathEl::LineTo(p) => {
                data.push(CMD_LINE_TO);
                data.push(p.x);
                data.push(p.y);
            }
            PathEl::CurveTo(p1, p2, p3) => {
                data.push(CMD_CURVE_TO);
                data.push(p1.x);
                data.push(p1.y);
                data.push(p2.x);
                data.push(p2.y);
                data.push(p3.x);
                data.push(p3.y);
            }
            PathEl::ClosePath => {
                data.push(CMD_CLOSE);
            }
            _ => {}
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-build a tiny 2-piece layout for unit tests that don't want
    /// to spin up the whole CVT pipeline. Two pieces on either side of
    /// one internal edge.
    fn two_piece_layout() -> PuzzleLayout {
        let mut bb = BezPath::new();
        bb.move_to(Point::new(0.0, 0.0));
        bb.line_to(Point::new(200.0, 0.0));
        bb.line_to(Point::new(200.0, 100.0));
        bb.line_to(Point::new(0.0, 100.0));
        bb.close_path();
        PuzzleLayout {
            width: 200.0,
            height: 100.0,
            outer_boundary: bb,
            edges: vec![LayoutEdge {
                start: Point::new(100.0, 0.0),
                end: Point::new(100.0, 100.0),
                direction: crate::edge::TabDirection::Out,
                connector: None,
                connector_params: None,
                pieces: (0, 1),
            }],
            pieces: vec![
                LayoutPiece {
                    id: 0,
                    center: Point::new(50.0, 50.0),
                    edge_indices: vec![0],
                    outline: None,
                },
                LayoutPiece {
                    id: 1,
                    center: Point::new(150.0, 50.0),
                    edge_indices: vec![0],
                    outline: None,
                },
            ],
            knob_base_cap: 0.0,
        }
    }

    #[test]
    fn shared_knob_controls_scale_and_offset_without_changing_the_mesh() {
        let mut heights = Vec::new();
        for size in [0.5, 1.0, 1.25] {
            let mut layout = two_piece_layout();
            layout.generate_connectors_with_style(&crate::ClassicKnobConnector, "shared-style", 0.0, size, 0.0);
            layout.compute_and_cap_avg_knob_base(&crate::ClassicKnobConnector);
            layout.resolve_knob_collisions(&crate::ClassicKnobConnector, 0.0, &[]);
            let edge = &layout.edges[0];
            assert_eq!(edge.start, Point::new(100.0, 0.0));
            assert_eq!(edge.end, Point::new(100.0, 100.0));
            let params = edge.connector_params.as_ref().unwrap();
            assert_eq!(params.knob_scale, size);
            assert_eq!(params.offset, 0.0);
            heights.push(edge.connector.as_ref().unwrap()[2].p3.y.abs());
        }
        assert!((heights[0] / heights[1] - 0.5).abs() < 1e-9);
        assert!((heights[2] / heights[1] - 1.25).abs() < 1e-9);

        let mut layout = two_piece_layout();
        layout.generate_connectors_with_style(&crate::ClassicKnobConnector, "shared-style", 0.0, 1.0, 1.0);
        assert!(layout.edges[0].connector_params.as_ref().unwrap().offset.abs() > 0.01);
        let svg = layout_generate_svg(&layout);
        let mut repeated = two_piece_layout();
        repeated.generate_connectors_with_style(&crate::ClassicKnobConnector, "shared-style", 0.0, 1.0, 1.0);
        assert_eq!(svg, layout_generate_svg(&repeated));
    }

    #[test]
    fn test_border_binary_starts_with_moveto_ends_with_close() {
        let layout = two_piece_layout();
        let data = layout_border_to_binary(&layout);
        assert!(!data.is_empty());
        assert_eq!(data[0], 0.0, "border starts with CMD_MOVE_TO");
        assert!(data.contains(&3.0), "border contains CMD_CLOSE");
    }

    #[test]
    fn test_edges_binary_empty_when_no_connectors() {
        // A layout with an edge but no connector renders as MoveTo +
        // LineTo — no curves.
        let layout = two_piece_layout();
        let data = layout_edges_to_binary(&layout);
        let mut i = 0;
        let mut moves = 0;
        let mut lines = 0;
        while i < data.len() {
            match data[i] {
                0.0 => { moves += 1; i += 3; }
                1.0 => { lines += 1; i += 3; }
                x => panic!("unexpected edge command {x} at index {i}"),
            }
        }
        assert_eq!(moves, 1);
        assert_eq!(lines, 1);
    }

    #[test]
    fn test_merge_short_edges_suppresses_the_short_one() {
        let mut layout = two_piece_layout();
        // Shorten the sole edge so it falls below threshold.
        layout.edges[0].start = Point::new(100.0, 0.0);
        layout.edges[0].end = Point::new(100.5, 0.0);
        layout.merge_short_edges(5.0);
        assert_eq!(layout.pieces.len(), 1, "pieces merged via short edge");
        assert_eq!(layout.edges.len(), 0, "short edge dropped");
    }

    #[test]
    fn test_remove_small_knobs_respects_min_per_piece() {
        // Build a triangle of 3 pieces around a shared central vertex,
        // each pair connected by one edge — so every piece has exactly
        // 2 edges. With min_knobs_per_piece = 2, no knobs should be
        // removable even if the edges are tiny, because any removal
        // would leave some piece with < 2 knobs.
        let boundary = {
            let mut p = BezPath::new();
            p.move_to(Point::new(0.0, 0.0));
            p.line_to(Point::new(300.0, 0.0));
            p.line_to(Point::new(150.0, 260.0));
            p.close_path();
            p
        };
        let mk_edge = |a: Point, b: Point, pieces: (usize, usize)| LayoutEdge {
            start: a,
            end: b,
            direction: crate::edge::TabDirection::Out,
            // Give every edge a placeholder connector so the removal
            // logic has something to strip.
            connector: Some(vec![kurbo::CubicBez::new(a, a, b, b)]),
            connector_params: Some(crate::edge::EdgeParams {
                length: (a - b).hypot(),
                cross_length: 10.0,
                direction: crate::edge::TabDirection::Out,
                offset: 0.0,
                knob_scale: 1.0,
            }),
            pieces,
        };
        let mut layout = PuzzleLayout {
            width: 300.0,
            height: 260.0,
            outer_boundary: boundary,
            pieces: vec![
                LayoutPiece { id: 0, center: Point::new(75.0, 80.0), edge_indices: vec![0, 2], outline: None },
                LayoutPiece { id: 1, center: Point::new(225.0, 80.0), edge_indices: vec![0, 1], outline: None },
                LayoutPiece { id: 2, center: Point::new(150.0, 180.0), edge_indices: vec![1, 2], outline: None },
            ],
            // One really short edge, two normal. Median = 5.
            edges: vec![
                mk_edge(Point::new(149.0, 1.0), Point::new(151.0, 1.0), (0, 1)), // length 2
                mk_edge(Point::new(200.0, 100.0), Point::new(205.0, 100.0), (1, 2)), // length 5
                mk_edge(Point::new(100.0, 100.0), Point::new(100.0, 105.0), (2, 0)), // length 5
            ],
            knob_base_cap: 0.0,
        };

        layout.remove_small_knobs(0.35, 2);

        // Every piece has exactly 2 edges, so no knob can be removed
        // without dropping below min_knobs_per_piece = 2.
        assert!(layout.edges.iter().all(|e| e.connector.is_some()));
    }

    #[test]
    fn test_remove_small_knobs_removes_when_piece_has_room() {
        // A piece with 4 edges, one obviously too short. With
        // min_knobs_per_piece = 2, the short one should get removed
        // because each adjacent piece has 3 other knobs.
        let boundary = {
            let mut p = BezPath::new();
            p.move_to(Point::new(0.0, 0.0));
            p.line_to(Point::new(400.0, 0.0));
            p.line_to(Point::new(400.0, 400.0));
            p.line_to(Point::new(0.0, 400.0));
            p.close_path();
            p
        };
        // 2×3 grid of pieces, 7 internal edges. The center-horizontal
        // edge is the short one.
        let long = 100.0;
        let mk = |a: Point, b: Point, pieces: (usize, usize)| LayoutEdge {
            start: a,
            end: b,
            direction: crate::edge::TabDirection::Out,
            connector: Some(vec![kurbo::CubicBez::new(a, a, b, b)]),
            connector_params: Some(crate::edge::EdgeParams {
                length: (a - b).hypot(),
                cross_length: 10.0,
                direction: crate::edge::TabDirection::Out,
                offset: 0.0,
                knob_scale: 1.0,
            }),
            pieces,
        };
        // 4 pieces at corners of a square — every piece has 2 edges of
        // length `long` (to immediate neighbors) and the center short
        // edge is between pieces 0 and 3 (diagonal? No, let's simplify).
        // Use 4 pieces in a row so each has either 1 or 2 edges.
        // Piece 0↔1 long, 1↔2 short, 2↔3 long, with piece 1 and 2
        // having an extra long edge to 0 and 3 respectively via the
        // synthetic "direct" edge.
        // Simpler: a + shape where a center piece has 4 edges.
        let mut layout = PuzzleLayout {
            width: 400.0,
            height: 400.0,
            outer_boundary: boundary,
            pieces: vec![
                // Center piece with 4 edges
                LayoutPiece { id: 0, center: Point::new(200.0, 200.0), edge_indices: vec![0, 1, 2, 3], outline: None },
                LayoutPiece { id: 1, center: Point::new(100.0, 200.0), edge_indices: vec![0, 4, 5], outline: None },
                LayoutPiece { id: 2, center: Point::new(300.0, 200.0), edge_indices: vec![1, 4, 6], outline: None },
                LayoutPiece { id: 3, center: Point::new(200.0, 100.0), edge_indices: vec![2, 5, 6], outline: None },
                LayoutPiece { id: 4, center: Point::new(200.0, 300.0), edge_indices: vec![3], outline: None },
            ],
            edges: vec![
                mk(Point::new(150.0, 200.0), Point::new(150.0, 200.0 + long), (0, 1)),
                mk(Point::new(250.0, 200.0), Point::new(250.0, 200.0 + long), (0, 2)),
                mk(Point::new(200.0, 150.0), Point::new(200.0 + long, 150.0), (0, 3)),
                // The short one on the bottom of center piece — adjacent
                // to piece 4 which is small but will still have 0 knobs
                // after removal. So it should NOT be removed.
                mk(Point::new(200.0, 250.0), Point::new(200.5, 250.0), (0, 4)),
                mk(Point::new(100.0, 250.0), Point::new(100.0 + long, 250.0), (1, 2)),
                mk(Point::new(150.0, 150.0), Point::new(150.0 + long, 150.0), (1, 3)),
                mk(Point::new(250.0, 150.0), Point::new(250.0 + long, 150.0), (2, 3)),
            ],
            knob_base_cap: 0.0,
        };

        layout.remove_small_knobs(0.35, 2);

        // Edge index 3 is the short one (length 0.5, median is long).
        // Piece 4 has only that edge; removing it would leave piece 4
        // with 0 knobs < 2. So it must be kept.
        assert!(layout.edges[3].connector.is_some(), "short edge kept because piece 4 only has this one");
    }

    #[test]
    fn test_merge_short_edges_noop_when_above_threshold() {
        let mut layout = two_piece_layout();
        let before_pieces = layout.pieces.len();
        let before_edges = layout.edges.len();
        layout.merge_short_edges(0.1);
        assert_eq!(layout.pieces.len(), before_pieces);
        assert_eq!(layout.edges.len(), before_edges);
    }
}
