use std::cell::RefCell;

use serde::Serialize;
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use web_sys::console;

use kurbo::{Affine, BezPath, Point, Shape, Vec2};

/// Returns `performance.now()` from the browser as a millisecond
/// timestamp. Native test builds get a constant stub — the helpers
/// fall through to no-op `log_phase` calls anyway, and touching
/// `web_sys::window()` from a non-wasm target panics inside
/// `js_sys` imported statics.
#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(f64::NAN)
}
#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    0.0
}

/// Log a phase-timing line to the browser console. No-op on native.
#[cfg(target_arch = "wasm32")]
fn log_phase(phase: &str, ms: f64) {
    console::log_1(&format!("[perf] {phase}: {ms:.1} ms").into());
}
#[cfg(not(target_arch = "wasm32"))]
fn log_phase(_: &str, _: f64) {}
use puzzle_core::{
    anchor_seeds_for_corner, arrow_path, bezpath_to_binary, build_classic_layout, build_cvt_layout, circle_path,
    diamond_path, finalize_cvt_layout, find_sharp_corners, heart_path, hexagon_path,
    layout_border_to_binary, layout_edges_to_binary,
    layout_generate_svg, mask_difference,
    rect_path, rounded_rect_path, star_path, triangle_path, ClassicKnobConnector, CvtParams,
    LayoutPiece, PuzzleConfig, PuzzleLayout, WhimsyPlacement, DEFAULT_FLATTEN_TOLERANCE,
    DEFAULT_LLOYD_ITERATIONS, DEFAULT_MERGE_EDGE_THRESHOLD, DEFAULT_MIN_PIECE_ANGLE_DEG,
    DEFAULT_SHARP_CORNER_ANGLE_DEG, DEFAULT_SMOOTH_ITERATIONS,
};

thread_local! {
    static CACHED_SVG: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Resolve a border/whimsy shape name to a closed `BezPath`. `"rectangle"`
/// (or an absent/None shape) produces a plain rectangle matching the
/// dimensions; named shapes are inscribed in the dimensions' bounding box.
/// Returns `Err` for unknown names.
fn resolve_boundary(
    shape: Option<&str>,
    width: f64,
    height: f64,
) -> Result<kurbo::BezPath, String> {
    let short = width.min(height);
    match shape {
        None | Some("") | Some("rectangle") => Ok(rect_path(width, height)),
        Some("rounded-rect") => Ok(rounded_rect_path(width, height, short * 0.1)),
        Some("circle") => Ok(circle_path(width, height)),
        Some("triangle") => Ok(triangle_path(width, height, short * 0.08)),
        Some("diamond") => Ok(diamond_path(width, height, short * 0.08)),
        Some("hexagon") => Ok(hexagon_path(width, height, short * 0.06)),
        Some("arrow") => Ok(arrow_path(width, height, short * 0.05)),
        Some("heart") => Ok(heart_path(width, height)),
        Some("star") => Ok(star_path(width, height, 5, short * 0.04)),
        Some(other) => Err(format!("Unknown border shape: {}", other)),
    }
}

/// Build a whimsy's outer contour in puzzle coordinates: shape is
/// constructed in its own local box `(0,0)..(w,h)`, translated so its
/// center lands at the origin, rotated, then translated to
/// `(center_x, center_y)`.
fn build_whimsy_path(w: &WhimsyPlacement) -> Result<BezPath, String> {
    let local = resolve_boundary(Some(&w.shape), w.width, w.height)
        .map_err(|e| format!("whimsy shape '{}': {e}", w.shape))?;
    let half = Vec2::new(w.width * 0.5, w.height * 0.5);
    let affine = Affine::translate(Vec2::new(w.center_x, w.center_y))
        * Affine::rotate(w.rotation.to_radians())
        * Affine::translate(-half);
    Ok(affine * local)
}

/// Output of `build_layout`: the layout to render plus the initial
/// anchor seed positions used for CVT (pre-Lloyd). The anchors are
/// returned alongside so the debug overlay can show where the seeds
/// *started* in addition to where they ended up (piece centers).
struct BuiltLayout {
    layout: PuzzleLayout,
    anchors: Vec<Point>,
}

/// Build the `PuzzleLayout` the rest of the pipeline renders from,
/// starting from a parsed and validated `PuzzleConfig`.
fn build_layout(config: &PuzzleConfig) -> Result<BuiltLayout, String> {
    if config.cell_algorithm == puzzle_core::CellAlgorithm::Classic {
        return Ok(BuiltLayout {
            layout: build_classic_layout(config)?,
            anchors: Vec::new(),
        });
    }
    let t_total = now_ms();
    let mut boundary =
        resolve_boundary(config.border_shape.as_deref(), config.width, config.height)?;


    // Pre-compute each whimsy's world-space contour, then subtract
    // each one from the outer boundary so the CVT cells hug the
    // whimsy's outline as their cut.
    let t = now_ms();
    let whimsy_paths: Vec<BezPath> = config
        .whimsies
        .iter()
        .map(build_whimsy_path)
        .collect::<Result<_, _>>()?;
    for wp in &whimsy_paths {
        boundary = mask_difference(&boundary, wp)
            .map_err(|e| format!("whimsy boundary subtraction: {e}"))?;
    }
    log_phase("whimsy_subtract", now_ms() - t);

    // Collect anchor seeds near every sharp whimsy corner so the
    // Voronoi bisector between each pair of anchors passes through
    // the corner — structurally preventing slivers before CVT runs.
    // Skip pairs that would fall outside the subtracted boundary
    // (e.g. a corner so close to another whimsy that the anchor
    // offset lands in another hole).
    let t = now_ms();
    let avg_piece_dim = ((config.width * config.height)
        / (config.piece_count as f64).max(1.0))
        .sqrt();
    let mut anchors: Vec<Point> = Vec::new();
    for wp in &whimsy_paths {
        for corner in find_sharp_corners(wp, DEFAULT_SHARP_CORNER_ANGLE_DEG) {
            if let Some((a, b)) = anchor_seeds_for_corner(&corner, &boundary, avg_piece_dim) {
                anchors.push(a);
                anchors.push(b);
            }
        }
    }
    log_phase("anchor_seeds", now_ms() - t);

    let lloyd_iters = match config.cell_algorithm {
        puzzle_core::CellAlgorithm::Classic => unreachable!("classic layouts are built above"),
        puzzle_core::CellAlgorithm::Cvt => DEFAULT_LLOYD_ITERATIONS,
        puzzle_core::CellAlgorithm::Poisson => {
            config.poisson_polish_iterations.min(10) as usize
        }
    };

    let params = CvtParams {
        width: config.width,
        height: config.height,
        boundary: &boundary,
        piece_count: config.piece_count as usize,
        seed: &config.seed,
        lloyd_iterations: lloyd_iters,
        boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
        merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
        // Every edge gets a knob during generation; we then prune
        // proportionally-small ones below via `remove_small_knobs`
        // which guarantees every piece keeps at least 2 knobs.
        min_knob_edge_length: 0.0,
        anchors: &anchors,
        min_piece_angle_deg: DEFAULT_MIN_PIECE_ANGLE_DEG,
        smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
        cell_algorithm: config.cell_algorithm,
    };
    let t = now_ms();
    let mut layout = build_cvt_layout(&params)?;
    log_phase("build_cvt_layout", now_ms() - t);

    let styled_knobs = config.knob_size != 1.0 || config.knob_randomness != 0.0;
    if styled_knobs {
        layout.generate_connectors_with_style(&ClassicKnobConnector, &config.seed, 0.0,
            config.knob_size, config.knob_randomness);
    }

    let t = now_ms();
    finalize_cvt_layout(&mut layout);
    log_phase("finalize_cvt_layout", now_ms() - t);

    // Relative (median-based) small-knob removal. 0.35 × median is a
    // starting tuning; crank higher to prune more aggressively.
    let t = now_ms();
    layout.remove_small_knobs(SMALL_KNOB_RATIO, MIN_KNOBS_PER_PIECE);
    log_phase("remove_small_knobs", now_ms() - t);

    // Optional: rebuild the silhouette so each cell's segment of the
    // outer boundary becomes a chord with a classic knob in its
    // middle. Runs after CVT so it sees the actual Voronoi-vs-boundary
    // clip points.
    if config.knob_outer_boundary && !config.disable_knobs {
        let t = now_ms();
        layout.knob_outer_boundary_with_style(&ClassicKnobConnector, &config.seed,
            config.knob_size, config.knob_randomness);
        log_phase("knob_outer_boundary", now_ms() - t);
    }
    if styled_knobs {
        layout.fit_styled_connectors_to_boundary();
    }
    // Debug toggle: clear every connector so each edge renders as a
    // plain straight cut. Lets the user inspect the raw CVT geometry
    // (slivers, boundary clipping) without knob noise.
    if config.disable_knobs {
        for edge in &mut layout.edges {
            edge.connector = None;
            edge.connector_params = None;
        }
    }

    // Append whimsies. `subdivisions` < 2 → a single solid pop-out
    // piece with no internal edges. `subdivisions >= 2` → run a nested
    // CVT inside the whimsy's contour, then merge its pieces and edges
    // into the main layout with offset indices.
    let t = now_ms();
    for (i, (w, path)) in config.whimsies.iter().zip(whimsy_paths).enumerate() {
        // voronoice requires ≥ 3 seeds to build a Delaunay triangulation;
        // anything lower collapses to a single solid whimsy piece.
        if w.subdivisions >= 3 {
            append_nested_cvt(&mut layout, w, &path, i, config)?;
        } else {
            let id = layout.pieces.len();
            layout.pieces.push(LayoutPiece {
                id,
                center: Point::new(w.center_x, w.center_y),
                edge_indices: Vec::new(),
                outline: Some(path),
            });
        }
    }
    log_phase("append_whimsies", now_ms() - t);
    log_phase("build_layout TOTAL", now_ms() - t_total);

    Ok(BuiltLayout { layout, anchors })
}

/// Run a CVT inside a whimsy's world-space contour and merge its
/// pieces + edges into `layout`, rewriting piece-ids and edge
/// references to account for the offset.
fn append_nested_cvt(
    layout: &mut PuzzleLayout,
    w: &WhimsyPlacement,
    path: &BezPath,
    index: usize,
    config: &PuzzleConfig,
) -> Result<(), String> {
    let bbox = path.bounding_box();
    let sub_seed = format!("{}-whimsy-{}", config.seed, index);
    let sub_params = CvtParams {
        width: bbox.width(),
        height: bbox.height(),
        boundary: path,
        piece_count: w.subdivisions as usize,
        seed: &sub_seed,
        lloyd_iterations: DEFAULT_LLOYD_ITERATIONS,
        boundary_flatten_tolerance: DEFAULT_FLATTEN_TOLERANCE,
        merge_edge_threshold: DEFAULT_MERGE_EDGE_THRESHOLD,
        min_knob_edge_length: 0.0,
        anchors: &[],
        // Nested CVTs preserve the exact caller-requested subdivision
        // count — don't sliver-merge a piece inside a whimsy just
        // because it touches the whimsy's own sharp features.
        min_piece_angle_deg: 0.0,
        smooth_iterations: DEFAULT_SMOOTH_ITERATIONS,
        // Nested CVTs always run the CVT algorithm regardless of
        // the parent puzzle's choice — they're internal subdivisions
        // (small piece counts, full Lloyd is cheap), and we don't
        // want a "Poisson on the parent + Poisson inside the whimsy"
        // double-noise look.
        cell_algorithm: puzzle_core::CellAlgorithm::Cvt,
    };
    let mut sub = build_cvt_layout(&sub_params)
        .map_err(|e| format!("whimsy[{index}] nested CVT: {e}"))?;
    let styled_knobs = config.knob_size != 1.0 || config.knob_randomness != 0.0;
    if styled_knobs {
        sub.generate_connectors_with_style(&ClassicKnobConnector, &sub_seed, 0.0,
            config.knob_size, config.knob_randomness);
    }
    finalize_cvt_layout(&mut sub);
    sub.remove_small_knobs(SMALL_KNOB_RATIO, MIN_KNOBS_PER_PIECE);
    if styled_knobs {
        sub.fit_styled_connectors_to_boundary();
    }
    if config.disable_knobs {
        for edge in &mut sub.edges {
            edge.connector = None;
            edge.connector_params = None;
        }
    }

    let piece_offset = layout.pieces.len();
    let edge_offset = layout.edges.len();

    for mut p in sub.pieces {
        p.id += piece_offset;
        for ei in p.edge_indices.iter_mut() {
            *ei += edge_offset;
        }
        layout.pieces.push(p);
    }
    for mut e in sub.edges {
        e.pieces = (e.pieces.0 + piece_offset, e.pieces.1 + piece_offset);
        layout.edges.push(e);
    }
    Ok(())
}

/// Knob removal threshold: edges shorter than this fraction of the
/// median edge length lose their connector (if doing so still leaves
/// both adjacent pieces with ≥ `MIN_KNOBS_PER_PIECE` knobs).
const SMALL_KNOB_RATIO: f64 = 0.35;

/// Every piece keeps at least this many knobbed edges so it remains
/// interlocked with its neighbors.
const MIN_KNOBS_PER_PIECE: usize = 2;

/// Parse a JSON config, filling in a default seed if empty, and
/// validating. Returns `Err` on JSON parse, validation, or (later) CVT
/// build errors.
fn parse_config(config_json: &str) -> Result<PuzzleConfig, String> {
    let mut config: PuzzleConfig =
        serde_json::from_str(config_json).map_err(|e| format!("Invalid JSON: {e}"))?;
    if config.seed.is_empty() {
        config.seed = "default".to_string();
    }
    config.validate()?;
    Ok(config)
}

/// Initialize the panic hook for better error messages in the browser console.
#[wasm_bindgen]
pub fn init_panic_hook() {
    console_error_panic_hook::set_once();
}

// ─── Response types ────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct GridResponse {
    piece_count: u32,
    width_mm: f64,
    height_mm: f64,
    seed: String,
    border_shape: Option<String>,
    edge_count: usize,
}

// ─── Public WASM endpoints ─────────────────────────────────────

/// Build a puzzle layout from a JSON configuration and return a JSON
/// summary.
///
/// Accepts PuzzleConfig JSON:
/// ```json
/// {
///   "piece_count": 48,
///   "width": 297.0,
///   "height": 210.0,
///   "unit": "Millimeters",
///   "seed": "my-puzzle-seed",
///   "border_shape": "heart"
/// }
/// ```
///
/// On error: `{"error": "message"}`.
#[wasm_bindgen]
pub fn generate_grid(config_json: &str) -> String {
    let config = match parse_config(config_json) {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")),
    };
    let layout = match build_layout(&config) {
        Ok(b) => b.layout,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")),
    };
    let response = GridResponse {
        piece_count: layout.pieces.len() as u32,
        width_mm: config.width,
        height_mm: config.height,
        seed: config.seed,
        border_shape: config.border_shape,
        edge_count: layout.edges.len(),
    };
    serde_json::to_string(&response)
        .unwrap_or_else(|e| format!(r#"{{"error":"Serialization error: {e}"}}"#))
}

/// Generate a laser-cutter-ready SVG from a JSON configuration string.
///
/// On error: `{"error": "message"}`
#[wasm_bindgen]
pub fn generate_svg(config_json: &str) -> String {
    let config = match parse_config(config_json) {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")),
    };
    let layout = match build_layout(&config) {
        Ok(b) => b.layout,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")),
    };
    layout_generate_svg(&layout)
}

/// Generate binary edge data for Canvas 2D rendering.
///
/// Returns a JS object:
/// - `edges`: Float64Array, command-prefixed internal edge commands
/// - `border`: Float64Array, command-prefixed outer boundary commands
/// - `width`, `height`: puzzle dims in mm
/// - `piece_count`: number of pieces after CVT (equal to config input)
///
/// Also caches the SVG string internally for retrieval via
/// `get_cached_svg()`.
///
/// On error: `{ error: "message" }`.
#[wasm_bindgen]
pub fn generate_edges_binary(config_json: &str) -> JsValue {
    let err_obj = |msg: &str| -> JsValue {
        let obj = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("error"),
            &JsValue::from_str(msg),
        );
        obj.into()
    };

    let config = match parse_config(config_json) {
        Ok(c) => c,
        Err(e) => return err_obj(&e),
    };
    let built = match build_layout(&config) {
        Ok(b) => b,
        Err(e) => return err_obj(&e),
    };
    let layout = built.layout;

    let edges_data = layout_edges_to_binary(&layout);
    let border_data = layout_border_to_binary(&layout);
    let svg = layout_generate_svg(&layout);
    CACHED_SVG.with(|c| *c.borrow_mut() = svg);

    let edges_arr = js_sys::Float64Array::new_with_length(edges_data.len() as u32);
    edges_arr.copy_from(&edges_data);
    let border_arr = js_sys::Float64Array::new_with_length(border_data.len() as u32);
    border_arr.copy_from(&border_data);

    // Interleaved [x0, y0, x1, y1, ...] for piece centers (final
    // post-Lloyd seed positions, merged-cell centroids where applicable)
    // and initial anchor positions (pre-Lloyd). Used by the debug
    // "Seeds" overlay in the frontend.
    let mut centers: Vec<f64> = Vec::with_capacity(layout.pieces.len() * 2);
    for p in &layout.pieces {
        centers.push(p.center.x);
        centers.push(p.center.y);
    }
    let centers_arr = js_sys::Float64Array::new_with_length(centers.len() as u32);
    centers_arr.copy_from(&centers);

    let mut anchors_flat: Vec<f64> = Vec::with_capacity(built.anchors.len() * 2);
    for a in &built.anchors {
        anchors_flat.push(a.x);
        anchors_flat.push(a.y);
    }
    let anchors_arr = js_sys::Float64Array::new_with_length(anchors_flat.len() as u32);
    anchors_arr.copy_from(&anchors_flat);

    let obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("edges"), &edges_arr);
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("border"), &border_arr);
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("centers"), &centers_arr);
    let _ = js_sys::Reflect::set(&obj, &JsValue::from_str("anchors"), &anchors_arr);
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("width"),
        &JsValue::from_f64(config.width),
    );
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("height"),
        &JsValue::from_f64(config.height),
    );
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("piece_count"),
        &JsValue::from_f64(layout.pieces.len() as f64),
    );
    if config.cell_algorithm == puzzle_core::CellAlgorithm::Classic {
        let (columns, rows) = puzzle_core::classic_grid_dimensions(
            config.piece_count,
            config.width,
            config.height,
        );
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("grid_columns"),
            &JsValue::from_f64(columns as f64),
        );
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("grid_rows"),
            &JsValue::from_f64(rows as f64),
        );
    }
    obj.into()
}

/// Retrieve the cached SVG string from the last
/// `generate_edges_binary()` call. Returns an empty string if no SVG
/// has been generated yet.
#[wasm_bindgen]
pub fn get_cached_svg() -> String {
    CACHED_SVG.with(|c| c.borrow().clone())
}

/// Return the command-prefixed binary path for a named whimsy shape
/// drawn in a unit 1 × 1 bounding box. The frontend caches these once
/// per shape and then applies its own affine transform (translate +
/// rotate + scale) to draw live ghost overlays during manipulation
/// without round-tripping to WASM every frame.
///
/// On unknown shape, returns an empty `Float64Array`.
#[wasm_bindgen]
pub fn get_shape_unit_path(shape: &str) -> js_sys::Float64Array {
    let path = match resolve_boundary(Some(shape), 1.0, 1.0) {
        Ok(p) => p,
        Err(_) => {
            return js_sys::Float64Array::new_with_length(0);
        }
    };
    let data = bezpath_to_binary(&path);
    let arr = js_sys::Float64Array::new_with_length(data.len() as u32);
    arr.copy_from(&data);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_knob_controls_work_for_both_voronoi_algorithms() {
        for algorithm in [puzzle_core::CellAlgorithm::Cvt, puzzle_core::CellAlgorithm::Poisson] {
            let mut config = PuzzleConfig {
                width: 400.0, height: 300.0, piece_count: 24,
                seed: "shared-knobs".into(), cell_algorithm: algorithm,
                ..PuzzleConfig::default()
            };
            let original = build_layout(&config).unwrap().layout;
            config.knob_size = 0.5;
            config.knob_randomness = 1.0;
            let styled = build_layout(&config).unwrap().layout;
            assert_eq!(styled.pieces.len(), original.pieces.len());
            assert_eq!(styled.edges.len(), original.edges.len());
            for (a, b) in original.edges.iter().zip(&styled.edges) {
                assert_eq!(a.start, b.start);
                assert_eq!(a.end, b.end);
                assert_eq!(a.pieces, b.pieces);
            }
            assert!(styled.edges.iter().any(|e| e.connector_params.as_ref().is_some_and(|p| p.offset.abs() > 0.01)));
            assert_ne!(layout_generate_svg(&original), layout_generate_svg(&styled));
            assert_eq!(layout_generate_svg(&styled), layout_generate_svg(&build_layout(&config).unwrap().layout));
            config.knob_size = 1.25;
            assert_ne!(layout_generate_svg(&styled), layout_generate_svg(&build_layout(&config).unwrap().layout));
        }
    }

    #[test]
    fn classic_config_aliases_preserve_existing_shared_designs() {
        let legacy = r#"{"piece_count":24,"width":400.0,"height":300.0,"unit":"Millimeters","seed":"alias","cell_algorithm":"classic","classic_knob_size":0.8,"classic_knob_randomness":0.7}"#;
        let current = legacy.replace("classic_knob_size", "knob_size").replace("classic_knob_randomness", "knob_randomness");
        assert_eq!(generate_svg(legacy), generate_svg(&current));
    }

    #[test]
    fn styled_knobs_respect_shaped_borders_and_subdivided_whimsies() {
        use kurbo::ParamCurve;
        for algorithm in [puzzle_core::CellAlgorithm::Cvt, puzzle_core::CellAlgorithm::Poisson] {
            for border in ["heart", "rectangle"] {
                let mut config = PuzzleConfig {
                    width: 500.0, height: 400.0, piece_count: 24,
                    seed: "styled-boundary".into(), cell_algorithm: algorithm,
                    border_shape: Some(border.into()), knob_size: 1.25, knob_randomness: 1.0,
                    ..PuzzleConfig::default()
                };
                let layout = build_layout(&config).unwrap().layout;
                assert!(layout.edges.iter().any(|e| e.connector.is_some()));
                for edge in &layout.edges {
                    let transform = puzzle_core::edge_transform(edge.start, edge.end);
                    if let Some(curves) = &edge.connector {
                        for curve in curves {
                            for i in 1..16 {
                                let point = transform * curve.eval(i as f64 / 16.0);
                                assert_ne!(layout.outer_boundary.winding(point), 0, "styled knob crosses {border} border");
                            }
                        }
                    }
                }
                config.border_shape = None;
                config.knob_size = 0.5;
                config.whimsies.push(WhimsyPlacement {
                    shape: "circle".into(), center_x: 250.0, center_y: 200.0,
                    width: 150.0, height: 150.0, rotation: 0.0, subdivisions: 4,
                });
                let layout = build_layout(&config).unwrap().layout;
                let nested_start = layout.pieces.len() - 4;
                assert!(layout.edges.iter().any(|e| e.pieces.0 >= nested_start && e.connector_params.as_ref().is_some_and(|p| p.knob_scale <= 0.5)));
                config.disable_knobs = true;
                config.knob_outer_boundary = true;
                let flat = build_layout(&config).unwrap().layout;
                assert!(flat.edges.iter().all(|e| e.connector.is_none()));
            }
        }
    }

    #[test]
    fn test_classic_grid_routes_to_four_sided_mesh_and_exports_svg() {
        for target in [47, 48] {
            let config_json = format!(r#"{{"piece_count":{target},"width":297.0,"height":210.0,"unit":"Millimeters","seed":"classic","cell_algorithm":"classic"}}"#);
            let result: serde_json::Value = serde_json::from_str(&generate_grid(&config_json)).unwrap();
            assert_eq!(result["piece_count"], 48, "{result}");
            assert_eq!(result["edge_count"], 82);
            let svg = generate_svg(&config_json);
            assert!(svg.starts_with("<svg"));
            assert!(svg.contains('C'));
            assert_eq!(svg.matches('M').count(), 83, "each shared cut is exported once");
        }
    }

    #[test]
    fn test_legacy_config_still_defaults_to_cvt() {
        let legacy = r#"{"piece_count":12,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"legacy"}"#;
        let mut explicit: serde_json::Value = serde_json::from_str(legacy).unwrap();
        explicit["cell_algorithm"] = "cvt".into();
        assert_eq!(generate_svg(legacy), generate_svg(&explicit.to_string()));
    }

    #[test]
    fn test_generate_grid_rectangular() {
        let config_json = r#"{"piece_count":48,"width":297.0,"height":210.0,"unit":"Millimeters","seed":"test"}"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["piece_count"], 48);
        assert_eq!(parsed["width_mm"], 297.0);
        assert_eq!(parsed["height_mm"], 210.0);
        assert!(parsed["edge_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn test_generate_grid_heart() {
        let config_json = r#"{"piece_count":48,"width":576.0,"height":432.0,"unit":"Millimeters","seed":"test","border_shape":"heart"}"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // The sliver-merge pass can absorb a piece at the heart's
        // bottom tip — accept anything close to 48.
        let count = parsed["piece_count"].as_u64().unwrap();
        assert!((45..=48).contains(&count), "piece_count = {count}");
        assert_eq!(parsed["border_shape"], "heart");
    }

    #[test]
    fn test_generate_grid_rectangle_explicit() {
        // border_shape="rectangle" explicit should behave like absent.
        let config_json = r#"{"piece_count":24,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"s","border_shape":"rectangle"}"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#));
    }

    #[test]
    fn test_generate_svg_returns_svg() {
        let config_json = r#"{"piece_count":24,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"svg-test"}"#;
        let result = generate_svg(config_json);
        assert!(result.starts_with("<svg"), "got: {}", &result[..50.min(result.len())]);
        assert!(result.contains("</svg>"));
    }

    #[test]
    fn test_generate_svg_has_curves() {
        let config_json = r#"{"piece_count":24,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"curves"}"#;
        let svg = generate_svg(config_json);
        let d_start = svg.find("d='").unwrap() + 3;
        let d_end = svg[d_start..].find('\'').unwrap() + d_start;
        let d = &svg[d_start..d_end];
        assert!(d.contains('C'), "expected cubic curves from knobs");
    }

    #[test]
    fn test_generate_svg_deterministic() {
        let config_json = r#"{"piece_count":24,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"determ"}"#;
        let a = generate_svg(config_json);
        let b = generate_svg(config_json);
        assert_eq!(a, b);
    }

    #[test]
    fn test_generate_svg_invalid_json() {
        let result = generate_svg("not valid");
        assert!(result.contains(r#""error""#));
    }

    #[test]
    fn test_border_shape_invalid_returns_error() {
        let config_json = r#"{"piece_count":24,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"s","border_shape":"dodecahedron"}"#;
        let result = generate_svg(config_json);
        assert!(result.contains(r#""error""#));
        assert!(result.contains("Unknown border shape"));
    }

    #[test]
    fn test_piece_count_too_low_returns_error() {
        let config_json = r#"{"piece_count":1,"width":200.0,"height":150.0,"unit":"Millimeters","seed":"s"}"#;
        let result = generate_svg(config_json);
        assert!(result.contains(r#""error""#));
    }

    #[test]
    fn test_whimsy_adds_a_piece_and_preserves_cvt_count() {
        // Place a small heart whimsy in the middle of a rectangular
        // puzzle. Total piece count = 24 CVT + 2 anchors near the
        // heart's sharp bottom tip + 1 whimsy piece, adjusted by any
        // sliver-merge absorption (typically ~27).
        let config_json = r#"{
            "piece_count":24,
            "width":400.0,
            "height":300.0,
            "unit":"Millimeters",
            "seed":"whimsy",
            "whimsies":[{"shape":"heart","center_x":200.0,"center_y":150.0,"width":60.0,"height":60.0}]
        }"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let count = parsed["piece_count"].as_u64().unwrap();
        assert!((24..=28).contains(&count), "piece_count = {count}");
    }

    #[test]
    fn test_whimsy_cut_appears_in_svg() {
        let config_json = r#"{
            "piece_count":24,
            "width":400.0,
            "height":300.0,
            "unit":"Millimeters",
            "seed":"whimsy-svg",
            "whimsies":[{"shape":"circle","center_x":200.0,"center_y":150.0,"width":80.0,"height":80.0}]
        }"#;
        let svg = generate_svg(config_json);
        assert!(svg.starts_with("<svg"));
        // The circle whimsy (subtracted from the boundary) should
        // introduce extra subpaths in the rendered border — multiple
        // `M` move-tos appear in the final path data.
        let d_start = svg.find("d='").unwrap() + 3;
        let d_end = svg[d_start..].find('\'').unwrap() + d_start;
        let d = &svg[d_start..d_end];
        let move_count = d.matches('M').count();
        assert!(
            move_count >= 2,
            "expected ≥2 MoveTo in path (outer + whimsy hole), got {move_count} in d='{d}'"
        );
    }

    #[test]
    fn test_whimsy_subdivisions_nest_cvt_inside_whimsy() {
        // Whimsy with subdivisions=4 should contribute 4 nested pieces
        // plus connector edges within the whimsy contour.
        let config_json = r#"{
            "piece_count":16,
            "width":400.0,
            "height":300.0,
            "unit":"Millimeters",
            "seed":"nested",
            "whimsies":[{"shape":"circle","center_x":200.0,"center_y":150.0,"width":160.0,"height":160.0,"subdivisions":4}]
        }"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // 16 CVT pieces in the frame + 4 nested pieces in the whimsy.
        assert_eq!(parsed["piece_count"], 20);
        // Edges should include nested-whimsy internal edges (more than
        // just the main-frame CVT edges).
        let edge_count = parsed["edge_count"].as_u64().unwrap();
        assert!(edge_count > 16, "expected nested whimsy edges, got {edge_count}");
    }

    #[test]
    fn test_whimsy_subdivisions_two_collapses_to_solid() {
        // voronoice can't triangulate 2 points — subdivisions<3 falls
        // back to a single solid whimsy piece.
        let config_json = r#"{
            "piece_count":12,
            "width":400.0,
            "height":300.0,
            "unit":"Millimeters",
            "seed":"s2",
            "whimsies":[{"shape":"circle","center_x":200.0,"center_y":150.0,"width":120.0,"height":120.0,"subdivisions":2}]
        }"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["piece_count"], 13);
    }

    #[test]
    fn test_whimsy_subdivisions_three_works() {
        let config_json = r#"{
            "piece_count":12,
            "width":400.0,
            "height":300.0,
            "unit":"Millimeters",
            "seed":"s3",
            "whimsies":[{"shape":"circle","center_x":200.0,"center_y":150.0,"width":120.0,"height":120.0,"subdivisions":3}]
        }"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#), "got: {result}");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["piece_count"], 15);
    }

    #[test]
    fn test_whimsy_invalid_shape_returns_error() {
        let config_json = r#"{
            "piece_count":12,
            "width":200.0,
            "height":150.0,
            "unit":"Millimeters",
            "seed":"s",
            "whimsies":[{"shape":"blob","center_x":100.0,"center_y":75.0,"width":40.0,"height":40.0}]
        }"#;
        let result = generate_grid(config_json);
        assert!(result.contains(r#""error""#), "got: {result}");
    }

    #[test]
    fn test_empty_seed_uses_default() {
        let config_json = r#"{"piece_count":12,"width":200.0,"height":150.0,"unit":"Millimeters","seed":""}"#;
        let result = generate_grid(config_json);
        assert!(!result.contains(r#""error""#));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["seed"], "default");
    }
}
