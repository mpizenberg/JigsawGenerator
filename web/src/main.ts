import {
  BUILD_SUPERSEDED,
  requestBuild,
  requestCachedSvg,
  requestShapeUnitPath,
} from "./worker-client";
import "./style.css";

function randomSeed(): string {
  return Math.random().toString(36).substring(2, 10);
}

/**
 * Minimum CVT edge length (mm) below which a knob's neck would be
 * thinner than ~3 mm — too fragile for laser-cut puzzle pieces.
 * Derived from the connector's fixed ratios (knob 0.25 × cell, neck
 * 0.25 × knob, so full neck opening = 0.125 × min_edge → need 24 mm
 * for 3 mm opening). Keep in sync with `DEFAULT_MIN_KNOB_EDGE_LENGTH`
 * in the Rust layer.
 */
const MIN_CELL_DIM_MM = 24;

/**
 * Linear dimension multiplier applied per-shape when computing the
 * required puzzle area for a target piece count. Combines two effects:
 *
 * - **Hex-vs-square cell**: CVT cells are roughly hexagonal, and a
 *   hex cell of the same area as an N×N square has edges shorter by
 *   ~0.62×. To hit a target *edge* length, the equivalent square side
 *   needs to be ~1.6× larger.
 * - **Shape fill**: heart/star/other non-rectangular shapes fill only
 *   ~50–60 % of their bounding rectangle, so the bbox needs to be
 *   correspondingly bigger for the shape's interior to hold cells of
 *   the required size.
 *
 * The values below include both effects. Tuned empirically against
 * `node /tmp/wasm_test.mjs`-style knob-coverage runs.
 */
const SHAPE_DIM_MULTIPLIER: Record<string, number> = {
  rectangle: 1.9,
  "rounded-rect": 1.95,
  circle: 2.15,
  hexagon: 2.2,
  triangle: 2.7,
  diamond: 2.7,
  arrow: 2.7,
  heart: 3.0,
  star: 3.0,
};
const DEFAULT_SHAPE_DIM_MULTIPLIER = 1.9;

function shapeDimMultiplier(): number {
  if (isClassicGrid()) return 100 / parseFloat(classicKnobSizeInput.value);
  const key = borderShapeSelect.value || "rectangle";
  return SHAPE_DIM_MULTIPLIER[key] ?? DEFAULT_SHAPE_DIM_MULTIPLIER;
}

// ─── DOM References ─────────────────────────────────────────

let widthInput: HTMLInputElement;
let heightInput: HTMLInputElement;
let unitSelect: HTMLSelectElement;
let seedInput: HTMLInputElement;
let pieceCount: HTMLElement;
let errorDisplay: HTMLElement;

let borderShapeSelect: HTMLSelectElement;
let cellAlgorithmSelect: HTMLSelectElement;
let poissonPolishSelect: HTMLSelectElement;
let poissonPolishGroup: HTMLElement;
let classicKnobSizeInput: HTMLInputElement;
let classicGridIrregularityInput: HTMLInputElement;
let classicKnobRandomnessInput: HTMLInputElement;

function classicStyleControls() {
  return [
    { input: classicKnobSizeInput, key: "classic_knob_size", url: "ks", defaultValue: 100 },
    { input: classicGridIrregularityInput, key: "classic_grid_irregularity", url: "gi", defaultValue: 0 },
    { input: classicKnobRandomnessInput, key: "classic_knob_randomness", url: "kr", defaultValue: 0 },
  ];
}

function syncClassicStyleControls(): void {
  for (const { input } of classicStyleControls()) {
    document.getElementById(`${input.id}-value`)!.textContent = `${input.value}%`;
    input.disabled = input !== classicGridIrregularityInput && !knobsEnabledCheckbox.checked;
  }
}

let pieceTargetInput: HTMLInputElement;
let pieceSizeWarning: HTMLElement;
let dimsLockCheckbox: HTMLInputElement;
let dimsLocked = false;
let knobsEnabledCheckbox: HTMLInputElement;
let edgeKnobsEnabledCheckbox: HTMLInputElement;
let seedsVisibleCheckbox: HTMLInputElement;

let whimsyList: HTMLUListElement;
let addWhimsyBtn: HTMLButtonElement;
let shapePicker: HTMLDialogElement;

// ─── Whimsy State ───────────────────────────────────────────

/**
 * One whimsy instance in the UI layer. Mapped to
 * `WhimsyPlacement` JSON when sent to the WASM layer.
 * `size` is a square bounding-box edge length in mm (width=height);
 * the UI keeps whimsies isotropic to simplify scale controls.
 */
interface WhimsyInstance {
  id: string;
  shape: string;
  centerX: number;
  centerY: number;
  size: number;
  rotation: number;
  subdivisions: number;
}

const whimsies: WhimsyInstance[] = [];
let whimsyIdCounter = 0;

/**
 * Shape-name → unit-box (1 × 1) command-prefixed path. Populated on
 * demand when a whimsy of that shape is first rendered. Shapes are
 * deterministic and dimension-independent (their rounded-corner radius
 * scales with `min(w, h)`), so caching the unit path and applying a
 * per-whimsy affine transform in the overlay renderer produces the same
 * pixels as re-building at the target size.
 */
const shapeUnitPathCache = new Map<string, Float64Array>();
const shapeUnitPathPending = new Set<string>();

/**
 * Synchronous accessor for shape unit paths used during canvas
 * rendering. The worker delivers paths asynchronously, so this
 * returns `undefined` on a cache miss and kicks off an async fetch;
 * when the fetch resolves, the path is cached and a re-render is
 * scheduled. The caller (whimsy ghost overlay) just skips drawing
 * the shape on miss — at most one frame of "shape not yet visible"
 * after the user adds a new whimsy type, which is imperceptible.
 */
function getShapeUnitPath(shape: string): Float64Array | undefined {
  const cached = shapeUnitPathCache.get(shape);
  if (cached !== undefined) return cached;
  if (!shapeUnitPathPending.has(shape)) {
    shapeUnitPathPending.add(shape);
    requestShapeUnitPath(shape)
      .then((path) => {
        shapeUnitPathCache.set(shape, path);
        shapeUnitPathPending.delete(shape);
        // Trigger a re-render so the newly-fetched shape appears.
        scheduleTransform();
      })
      .catch((err) => {
        shapeUnitPathPending.delete(shape);
        console.warn(`[shape '${shape}'] fetch failed:`, err);
      });
  }
  return undefined;
}

let rulerHCanvas: HTMLCanvasElement;
let rulerVCanvas: HTMLCanvasElement;
let rulerHCtx: CanvasRenderingContext2D | null = null;
let rulerVCtx: CanvasRenderingContext2D | null = null;
let svgViewport: HTMLElement;
let zoomLevelDisplay: HTMLElement;
let zoomInBtn: HTMLElement;
let zoomOutBtn: HTMLElement;
let zoomResetBtn: HTMLElement;

// ─── Zoom/Pan State ──────────────────────────────────────────

let zoomLevel = 1;
let panX = 0;
let panY = 0;

let rafPending = false;

// ─── Canvas Interaction State ───────────────────────────────

/**
 * Mutually-exclusive interaction modes for mouse/touch drags on the
 * puzzle canvas. One active mode at a time; `mousedown` picks a mode
 * based on what's under the cursor, `mousemove` updates geometry, and
 * `mouseup` returns to `idle` (and triggers a CVT regen if the
 * interaction was a whimsy manipulation).
 */
type CornerDir = "tl" | "tr" | "br" | "bl";
type InteractionMode =
  | { kind: "idle" }
  | { kind: "panning"; startX: number; startY: number }
  | {
      kind: "dragging-whimsy";
      id: string;
      offsetX: number;
      offsetY: number;
      // Original whimsy center at mousedown — used to gate regen-on-
      // release until the user has actually moved the whimsy past
      // `DRAG_REGEN_THRESHOLD_MM`. Without this, every click-without-
      // drag would regen the layout on mouseup.
      startCenterX: number;
      startCenterY: number;
      // Latched once the threshold is crossed; mouseup regens iff true.
      committed: boolean;
    }
  | {
      kind: "scaling-whimsy";
      id: string;
      corner: CornerDir;
      initialSize: number;
      initialDist: number;
      // Same gating logic as drag — a tiny corner-wiggle shouldn't
      // trigger a full regen.
      committed: boolean;
    }
  | {
      kind: "rotating-whimsy";
      id: string;
      initialRotation: number;
      initialAngleDeg: number;
      committed: boolean;
    };

/// Minimum displacement (mm) before a whimsy drag triggers a layout
/// regen on mouseup. The user can click on a whimsy and release without
/// dragging — nothing about the puzzle has actually changed, so the
/// canonical layout should stay on screen unchanged.
const DRAG_REGEN_THRESHOLD_MM = 1.5;
/// Same idea for scale: minimum size delta (relative) before regen.
const SCALE_REGEN_THRESHOLD_RATIO = 0.05;
/// Same for rotation: minimum angle delta (deg) before regen.
const ROTATE_REGEN_THRESHOLD_DEG = 2.0;

let interaction: InteractionMode = { kind: "idle" };
let selectedWhimsyId: string | null = null;

const HANDLE_HIT_RADIUS_PX = 10;
const HANDLE_SIZE_PX = 8;

const MIN_ZOOM = 0.5;
const MAX_ZOOM = 20;
const ZOOM_STEP = 1.15; // 15% per wheel tick

// ─── Canvas State ────────────────────────────────────────────

let canvas: HTMLCanvasElement | null = null;
let ctx: CanvasRenderingContext2D | null = null;
let edgesData: Float64Array | null = null;
let borderData: Float64Array | null = null;
let centersData: Float64Array | null = null;
let anchorsData: Float64Array | null = null;
// Cached Path2D objects built once per puzzle generation. Re-stroking
// a Path2D on pan/zoom is O(1) on the JS side regardless of how many
// curves are inside — vs. re-walking the binary command stream every
// frame, which is O(curves). At 5k pieces ≈ 75k Béziers, this turns
// pan/zoom from chunky into instant.
let edgesPath2D: Path2D | null = null;
let borderPath2D: Path2D | null = null;
let puzzleWidth = 0;
let puzzleHeight = 0;
let generatedPieceCount = 0;

// ─── Config Builder ─────────────────────────────────────────

let savedVoronoiBorder: string | null = null;

function isClassicGrid(): boolean {
  return cellAlgorithmSelect?.value === "classic";
}

/** Preserve Voronoi-only settings when switching to a classic grid. */
function syncAlgorithmControls(): void {
  if (!poissonPolishGroup || !cellAlgorithmSelect) return;
  const isPoisson = cellAlgorithmSelect.value === "poisson";
  poissonPolishGroup.hidden = !isPoisson;
  const classic = isClassicGrid();
  document.getElementById("classic-grid-note")!.hidden = !classic;
  document.getElementById("classic-style-controls")!.hidden = !classic;
  document.getElementById("whimsies-section")!.hidden = classic;
  if (classic) {
    if (savedVoronoiBorder === null) savedVoronoiBorder = borderShapeSelect.value;
    borderShapeSelect.value = "rectangle";
    selectedWhimsyId = null;
    interaction = { kind: "idle" };
  } else if (savedVoronoiBorder !== null) {
    borderShapeSelect.value = savedVoronoiBorder;
    savedVoronoiBorder = null;
  }
  borderShapeSelect.disabled = classic;
}

function buildConfig(): object {
  const config: Record<string, unknown> = {
    piece_count: parseInt(pieceTargetInput.value, 10) || 48,
    width: parseFloat(widthInput.value),
    height: parseFloat(heightInput.value),
    unit: unitSelect.value,
    seed: seedInput.value,
  };
  const borderVal = borderShapeSelect.value;
  if (borderVal) {
    config.border_shape = borderVal;
  }
  // Cell-generation algorithm. Omitted from the JSON when set to the
  // default ("cvt") so existing URLs keep producing identical output.
  const algoVal = cellAlgorithmSelect?.value;
  if (algoVal && algoVal !== "cvt") {
    config.cell_algorithm = algoVal;
  }
  if (isClassicGrid()) {
    for (const { input, key } of classicStyleControls()) {
      config[key] = parseFloat(input.value) / 100;
    }
  }
  // Poisson polish count: only meaningful when algorithm is poisson.
  // Always include in the config when poisson is selected so the
  // backend default doesn't override the user's choice.
  if (algoVal === "poisson" && poissonPolishSelect) {
    const polish = parseInt(poissonPolishSelect.value, 10);
    if (!Number.isNaN(polish)) {
      config.poisson_polish_iterations = polish;
    }
  }
  if (knobsEnabledCheckbox && !knobsEnabledCheckbox.checked) {
    config.disable_knobs = true;
  }
  if (edgeKnobsEnabledCheckbox && edgeKnobsEnabledCheckbox.checked) {
    config.knob_outer_boundary = true;
  }
  if (!isClassicGrid() && whimsies.length > 0) {
    config.whimsies = whimsies.map((w) => ({
      shape: w.shape,
      center_x: w.centerX,
      center_y: w.centerY,
      width: w.size,
      height: w.size,
      rotation: w.rotation,
      subdivisions: w.subdivisions,
    }));
  }
  return config;
}

// ─── Whimsy Helpers ─────────────────────────────────────────

/**
 * Default size for a newly-added whimsy, in mm. Aim for a shape large
 * enough to be noticeably bigger than a single CVT piece but small
 * enough to leave breathing room inside the border.
 */
function defaultWhimsySize(): number {
  const w = parseFloat(widthInput.value) || 100;
  const h = parseFloat(heightInput.value) || 100;
  const pc = parseInt(pieceTargetInput.value, 10) || 48;
  const avgPieceDim = Math.sqrt((w * h) / pc);
  const short = Math.min(w, h);
  // 2× a single piece dim floor; 30 % of short side ceiling;
  // 20 % of short side is the nominal target.
  return Math.min(short * 0.3, Math.max(2 * avgPieceDim, short * 0.2));
}

function defaultWhimsyCenter(): { x: number; y: number } {
  const w = parseFloat(widthInput.value) || 0;
  const h = parseFloat(heightInput.value) || 0;
  return { x: w / 2, y: h / 2 };
}

/**
 * Minimum gap (mm) between any two whimsy outlines (their actual
 * shape geometry, not bbox), and between a whimsy outline and the
 * puzzle's outer rectangle. Prevents the CVT pipeline from being
 * given a configuration where two whimsy holes (or a whimsy and the
 * border) carve the puzzle interior into a thin strip — which
 * produces slivers and pinches the anchor seed placements.
 */
const WHIMSY_CLEARANCE_MM = 1.25;

/** Cubic-bezier subdivision count when flattening a whimsy outline. */
const POLY_FLATTEN_STEPS = 8;

/**
 * Flatten a whimsy's cached unit-box path (`getShapeUnitPath`) to a
 * world-space polygon — its `shape × size × rotation × position`
 * outline as a list of (x, y) vertices. Cubic-bezier segments are
 * subdivided into `POLY_FLATTEN_STEPS` linear pieces.
 *
 * The transform mirrors the one applied in `drawSelectionOverlay`:
 * `translate(centerX, centerY) · rotate(rotation) · scale(size) · translate(-0.5, -0.5)`.
 */
function whimsyPolygon(w: WhimsyInstance): { x: number; y: number }[] {
  const unitPath = getShapeUnitPath(w.shape);
  if (unitPath === undefined || unitPath.length === 0) return [];

  const rad = (w.rotation * Math.PI) / 180;
  const cos = Math.cos(rad);
  const sin = Math.sin(rad);
  const transform = (lx: number, ly: number) => {
    const tx = (lx - 0.5) * w.size;
    const ty = (ly - 0.5) * w.size;
    return {
      x: w.centerX + tx * cos - ty * sin,
      y: w.centerY + tx * sin + ty * cos,
    };
  };

  const out: { x: number; y: number }[] = [];
  let i = 0;
  let lastX = 0;
  let lastY = 0;
  while (i < unitPath.length) {
    const cmd = unitPath[i];
    if (cmd === 0 /* moveTo */) {
      lastX = unitPath[i + 1];
      lastY = unitPath[i + 2];
      out.push(transform(lastX, lastY));
      i += 3;
    } else if (cmd === 1 /* lineTo */) {
      lastX = unitPath[i + 1];
      lastY = unitPath[i + 2];
      out.push(transform(lastX, lastY));
      i += 3;
    } else if (cmd === 2 /* curveTo */) {
      const c1x = unitPath[i + 1];
      const c1y = unitPath[i + 2];
      const c2x = unitPath[i + 3];
      const c2y = unitPath[i + 4];
      const ex = unitPath[i + 5];
      const ey = unitPath[i + 6];
      for (let k = 1; k <= POLY_FLATTEN_STEPS; k++) {
        const t = k / POLY_FLATTEN_STEPS;
        const u = 1 - t;
        const px =
          u * u * u * lastX +
          3 * u * u * t * c1x +
          3 * u * t * t * c2x +
          t * t * t * ex;
        const py =
          u * u * u * lastY +
          3 * u * u * t * c1y +
          3 * u * t * t * c2y +
          t * t * t * ey;
        out.push(transform(px, py));
      }
      lastX = ex;
      lastY = ey;
      i += 7;
    } else if (cmd === 3 /* close */) {
      i += 1;
    } else {
      i += 1;
    }
  }
  return out;
}

/** Distance from point `(px, py)` to segment `(ax,ay)–(bx,by)`. */
function pointSegmentDistance(
  px: number,
  py: number,
  ax: number,
  ay: number,
  bx: number,
  by: number,
): number {
  const dx = bx - ax;
  const dy = by - ay;
  const len2 = dx * dx + dy * dy;
  let t = len2 > 0 ? ((px - ax) * dx + (py - ay) * dy) / len2 : 0;
  if (t < 0) t = 0;
  else if (t > 1) t = 1;
  const cx = ax + t * dx;
  const cy = ay + t * dy;
  return Math.hypot(px - cx, py - cy);
}

/** Even-odd point-in-polygon test (treats `poly` as a closed loop). */
function pointInPolygon(
  px: number,
  py: number,
  poly: { x: number; y: number }[],
): boolean {
  let inside = false;
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const xi = poly[i].x;
    const yi = poly[i].y;
    const xj = poly[j].x;
    const yj = poly[j].y;
    const intersects =
      yi > py !== yj > py &&
      px < ((xj - xi) * (py - yi)) / (yj - yi) + xi;
    if (intersects) inside = !inside;
  }
  return inside;
}

/**
 * Minimum distance between two closed polygons. Returns 0 if they
 * overlap (any vertex of one inside the other). Otherwise: minimum of
 * (each A-vertex to each B-segment) and (each B-vertex to each
 * A-segment). With 30-ish vertices per shape the whole thing is
 * ~1800 operations — fine for per-frame drag updates.
 */
function minPolyDistance(
  a: { x: number; y: number }[],
  b: { x: number; y: number }[],
): number {
  if (a.length === 0 || b.length === 0) return Infinity;
  // Quick overlap test: any vertex of either polygon inside the other.
  for (const p of a) if (pointInPolygon(p.x, p.y, b)) return 0;
  for (const p of b) if (pointInPolygon(p.x, p.y, a)) return 0;

  let best = Infinity;
  for (const p of a) {
    for (let j = 0; j < b.length; j++) {
      const q1 = b[j];
      const q2 = b[(j + 1) % b.length];
      const d = pointSegmentDistance(p.x, p.y, q1.x, q1.y, q2.x, q2.y);
      if (d < best) best = d;
    }
  }
  for (const p of b) {
    for (let j = 0; j < a.length; j++) {
      const q1 = a[j];
      const q2 = a[(j + 1) % a.length];
      const d = pointSegmentDistance(p.x, p.y, q1.x, q1.y, q2.x, q2.y);
      if (d < best) best = d;
    }
  }
  return best;
}

/**
 * Returns true if placing/updating `candidate` at its current
 * position keeps it (a) at least `WHIMSY_CLEARANCE_MM` away from every
 * other whimsy's outline and (b) at least `WHIMSY_CLEARANCE_MM` away
 * from every side of the puzzle's outer rectangle. Both checks use
 * the actual shape geometry (flattened), not the AABB.
 */
function whimsyPlacementValid(
  candidate: WhimsyInstance,
  others: WhimsyInstance[],
): boolean {
  const puzzleW = parseFloat(widthInput.value) || 0;
  const puzzleH = parseFloat(heightInput.value) || 0;
  if (puzzleW <= 0 || puzzleH <= 0) return true;

  const candPoly = whimsyPolygon(candidate);
  if (candPoly.length === 0) return true; // unknown shape — no check possible

  // Edge clearance: every outline vertex must sit at least
  // WHIMSY_CLEARANCE_MM inside each puzzle-rect side.
  const inset = WHIMSY_CLEARANCE_MM;
  for (const p of candPoly) {
    if (
      p.x < inset ||
      p.y < inset ||
      p.x > puzzleW - inset ||
      p.y > puzzleH - inset
    ) {
      return false;
    }
  }
  // Whimsy-to-whimsy clearance via shape geometry.
  for (const other of others) {
    if (other.id === candidate.id) continue;
    const otherPoly = whimsyPolygon(other);
    if (otherPoly.length === 0) continue;
    if (minPolyDistance(candPoly, otherPoly) < WHIMSY_CLEARANCE_MM) {
      return false;
    }
  }
  return true;
}

function addWhimsy(shape: string): void {
  const size = defaultWhimsySize();
  const { x, y } = defaultWhimsyCenter();
  const candidate: WhimsyInstance = {
    id: `w${whimsyIdCounter++}`,
    shape,
    centerX: x,
    centerY: y,
    size,
    rotation: 0,
    subdivisions: 0,
  };

  // If the default center conflicts with an existing whimsy or is
  // too close to the edge, spiral outward through a few candidate
  // positions. Falls back to the default position if nothing fits.
  if (!whimsyPlacementValid(candidate, whimsies)) {
    const offsets = [size, size * 1.5, size * 2.0, size * 2.5];
    const angles = [0, 60, 120, 180, 240, 300, 30, 90, 150, 210, 270, 330];
    let placed = false;
    outer: for (const off of offsets) {
      for (const angDeg of angles) {
        const rad = (angDeg * Math.PI) / 180;
        candidate.centerX = x + off * Math.cos(rad);
        candidate.centerY = y + off * Math.sin(rad);
        if (whimsyPlacementValid(candidate, whimsies)) {
          placed = true;
          break outer;
        }
      }
    }
    if (!placed) {
      // Couldn't find a valid spot; restore default and let the user
      // resize / drag manually.
      candidate.centerX = x;
      candidate.centerY = y;
    }
  }

  whimsies.push(candidate);
  renderWhimsies();
  scheduleGenerate();
}

function removeWhimsy(id: string): void {
  const idx = whimsies.findIndex((w) => w.id === id);
  if (idx >= 0) {
    whimsies.splice(idx, 1);
    renderWhimsies();
    scheduleGenerate();
  }
}

function renderWhimsies(): void {
  whimsyList.innerHTML = "";
  for (const w of whimsies) {
    whimsyList.appendChild(buildWhimsyCard(w));
  }
}

function buildWhimsyCard(w: WhimsyInstance): HTMLLIElement {
  const short = Math.min(
    parseFloat(widthInput.value) || 100,
    parseFloat(heightInput.value) || 100,
  );
  const sizeMin = Math.max(10, Math.round(short * 0.05));
  const sizeMax = Math.max(sizeMin + 1, Math.round(short * 0.6));

  const li = document.createElement("li");
  li.className = "whimsy-card";
  li.dataset.id = w.id;
  li.innerHTML = `
    <div class="whimsy-card-header">
      <span class="whimsy-name">${w.shape}</span>
      <button type="button" class="whimsy-delete" title="Delete whimsy">&times;</button>
    </div>
    <div class="whimsy-control">
      <label>Size</label>
      <input type="range" class="whimsy-size" min="${sizeMin}" max="${sizeMax}" step="1" value="${Math.round(w.size)}"/>
      <span class="value-readout">${Math.round(w.size)} mm</span>
    </div>
    <div class="whimsy-control">
      <label>Rotation</label>
      <input type="range" class="whimsy-rotation" min="0" max="360" step="1" value="${Math.round(w.rotation)}"/>
      <span class="value-readout">${Math.round(w.rotation)}°</span>
    </div>
    <div class="whimsy-control">
      <label>Subdivisions</label>
      <input type="number" class="whimsy-subdivisions" min="0" max="40" step="1" value="${w.subdivisions}" title="0 = solid; 3+ = nested CVT"/>
    </div>
  `;

  const sizeSlider = li.querySelector(".whimsy-size") as HTMLInputElement;
  const sizeReadout = sizeSlider.nextElementSibling as HTMLElement;
  sizeSlider.addEventListener("input", () => {
    const next = parseFloat(sizeSlider.value);
    const candidate: WhimsyInstance = { ...w, size: next };
    if (whimsyPlacementValid(candidate, whimsies)) {
      w.size = next;
      sizeReadout.textContent = `${Math.round(w.size)} mm`;
      scheduleGenerate();
    } else {
      // Snap the slider back to the last legal value.
      sizeSlider.value = String(Math.round(w.size));
    }
  });

  const rotSlider = li.querySelector(".whimsy-rotation") as HTMLInputElement;
  const rotReadout = rotSlider.nextElementSibling as HTMLElement;
  rotSlider.addEventListener("input", () => {
    const next = parseFloat(rotSlider.value);
    const candidate: WhimsyInstance = { ...w, rotation: next };
    if (whimsyPlacementValid(candidate, whimsies)) {
      w.rotation = next;
      rotReadout.textContent = `${Math.round(w.rotation)}°`;
      scheduleGenerate();
    } else {
      rotSlider.value = String(Math.round(w.rotation));
    }
  });

  const subInput = li.querySelector(".whimsy-subdivisions") as HTMLInputElement;
  subInput.addEventListener("input", () => {
    const v = parseInt(subInput.value, 10);
    // 1 and 2 can't form a valid nested CVT (voronoice needs ≥ 3
    // seeds); treat them as 0 (solid whimsy) so the user sees a
    // predictable fallback rather than a silent collapse.
    w.subdivisions = isNaN(v) || v < 0 || v === 1 || v === 2 ? 0 : v;
    scheduleGenerate();
  });

  const deleteBtn = li.querySelector(".whimsy-delete") as HTMLButtonElement;
  deleteBtn.addEventListener("click", () => removeWhimsy(w.id));

  return li;
}

// ─── URL Param Sync ──────────────────────────────────────────

/** Round a floating value to one decimal place for compact URL encoding. */
function fmtShort(n: number): string {
  return String(Math.round(n * 10) / 10);
}

function loadFromURL(): boolean {
  const params = new URLSearchParams(window.location.search);
  if (params.size === 0) return false;

  // Piece count: prefer `pc`, fall back to legacy `rows`·`cols` URLs.
  let pc = parseInt(params.get("pc") ?? "", 10);
  if (isNaN(pc)) {
    const rows = parseInt(params.get("rows") ?? "", 10);
    const cols = parseInt(params.get("cols") ?? "", 10);
    pc = !isNaN(rows) && !isNaN(cols) ? rows * cols : 48;
  }
  const w = parseFloat(params.get("w") ?? "297");
  const h = parseFloat(params.get("h") ?? "210");
  const unitParam = params.get("unit") ?? "mm";
  const unit = unitParam === "in" ? "Inches" : "Millimeters";
  const seed = params.get("seed") ?? "";
  // Legacy URLs stored missing `border` for rectangle; treat as
  // "rectangle" now that rectangle is an explicit option.
  const border = params.get("border") ?? "rectangle";

  pieceTargetInput.value = String(pc);
  widthInput.value = String(w);
  heightInput.value = String(h);
  unitSelect.value = unit;
  seedInput.value = seed || randomSeed();
  borderShapeSelect.value = border || "rectangle";
  // Cell-generation algorithm. Defaults to "cvt" so legacy URLs
  // without `algo=` keep producing identical output.
  cellAlgorithmSelect.value = params.get("algo") ?? "cvt";
  for (const { input, url, defaultValue } of classicStyleControls()) {
    const raw = params.get(url);
    const value = raw === null ? defaultValue : Number(raw);
    input.value = String(Number.isFinite(value)
      ? Math.max(Number(input.min), Math.min(Number(input.max), value))
      : defaultValue);
  }
  // Poisson polish iterations. Defaults to 3 (matches backend
  // default). Range clamped 0–10. Ignored when algo != poisson.
  const polishStr = params.get("polish");
  if (polishStr !== null) {
    const polish = parseInt(polishStr, 10);
    if (!Number.isNaN(polish)) {
      poissonPolishSelect.value = String(Math.max(0, Math.min(10, polish)));
    }
  }
  syncAlgorithmControls();

  // Whimsies: `wh=heart:200,150,60,0,0;star:100,80,40,45,3`
  // Each whimsy is `shape:cx,cy,size,rotation,subdivisions`,
  // semicolon-separated. Malformed entries are skipped.
  whimsies.length = 0;
  const whParam = params.get("wh");
  if (whParam) {
    for (const chunk of whParam.split(";")) {
      if (!chunk) continue;
      const colonIdx = chunk.indexOf(":");
      if (colonIdx < 0) continue;
      const shape = chunk.slice(0, colonIdx);
      const fields = chunk.slice(colonIdx + 1).split(",");
      if (fields.length < 5) continue;
      const cx = parseFloat(fields[0]);
      const cy = parseFloat(fields[1]);
      const size = parseFloat(fields[2]);
      const rot = parseFloat(fields[3]);
      const subs = parseInt(fields[4], 10);
      if ([cx, cy, size, rot].some((v) => isNaN(v)) || isNaN(subs)) continue;
      whimsies.push({
        id: `w${whimsyIdCounter++}`,
        shape,
        centerX: cx,
        centerY: cy,
        size,
        rotation: rot,
        subdivisions: subs,
      });
    }
  }

  return true;
}

function updateURL(): void {
  const config = buildConfig() as Record<string, unknown>;
  const params = new URLSearchParams();
  params.set("pc", String(config.piece_count));
  params.set("w", String(config.width));
  params.set("h", String(config.height));
  params.set("unit", config.unit === "Inches" ? "in" : "mm");
  params.set("seed", String(config.seed));
  const borderVal = borderShapeSelect.value;
  if (borderVal && borderVal !== "rectangle") {
    params.set("border", borderVal);
  }
  // Cell algorithm. Omitted when default ("cvt") so existing URLs
  // stay byte-identical.
  const algoVal = cellAlgorithmSelect.value;
  if (algoVal && algoVal !== "cvt") {
    params.set("algo", algoVal);
  }
  if (isClassicGrid()) {
    for (const { input, url, defaultValue } of classicStyleControls()) {
      if (Number(input.value) !== defaultValue) params.set(url, input.value);
    }
  }
  // Poisson polish count. Only included when (a) algorithm is
  // poisson and (b) the value isn't the default 3 — keeps URLs
  // short for the common case.
  if (algoVal === "poisson") {
    const polish = poissonPolishSelect.value;
    if (polish && polish !== "3") {
      params.set("polish", polish);
    }
  }
  if (!isClassicGrid() && whimsies.length > 0) {
    const encoded = whimsies
      .map(
        (w) =>
          `${w.shape}:${fmtShort(w.centerX)},${fmtShort(w.centerY)},${fmtShort(
            w.size,
          )},${Math.round(w.rotation)},${w.subdivisions}`,
      )
      .join(";");
    params.set("wh", encoded);
  }
  history.replaceState(null, "", "?" + params.toString());
}

// ─── Debounced URL Sync ──────────────────────────────────────

let urlTimeout: ReturnType<typeof setTimeout> | null = null;
function scheduleURLUpdate(): void {
  if (urlTimeout !== null) clearTimeout(urlTimeout);
  urlTimeout = setTimeout(updateURL, 300);
}

// ─── Canvas Resize ───────────────────────────────────────────

function resizeCanvasToViewport(
  el: HTMLCanvasElement,
  ctx2d: CanvasRenderingContext2D | null,
  cssWidth: number,
  cssHeight: number,
): void {
  const dpr = window.devicePixelRatio || 1;
  el.width = Math.max(1, Math.round(cssWidth * dpr));
  el.height = Math.max(1, Math.round(cssHeight * dpr));
  el.style.width = cssWidth + "px";
  el.style.height = cssHeight + "px";
  if (ctx2d) ctx2d.setTransform(dpr, 0, 0, dpr, 0, 0);
}

function resizeCanvas(): void {
  if (!canvas || !ctx) return;
  const rect = svgViewport.getBoundingClientRect();
  resizeCanvasToViewport(canvas, ctx, rect.width, rect.height);
  // Match rulers to the viewport so ticks line up with the puzzle canvas.
  if (rulerHCanvas && rulerHCtx) {
    const r = rulerHCanvas.getBoundingClientRect();
    resizeCanvasToViewport(rulerHCanvas, rulerHCtx, r.width, r.height);
  }
  if (rulerVCanvas && rulerVCtx) {
    const r = rulerVCanvas.getBoundingClientRect();
    resizeCanvasToViewport(rulerVCanvas, rulerVCtx, r.width, r.height);
  }
}

// ─── Rulers ─────────────────────────────────────────────────

/**
 * Pick a "nice" major-tick step (mm) such that the tick spacing on
 * screen is at least `minPxBetweenMajorTicks`. Steps are a 1-2-5 ladder
 * in mm — so at tight zooms we get 1 mm major ticks, loose zooms step
 * up to 2, 5, 10, 20, 50, 100, 200, 500, 1000 mm. In inch mode the
 * ladder adapts to 1-2-5 inch-equivalents.
 */
function pickMajorStep(scale: number, minPxBetweenMajorTicks: number): number {
  const inches = unitSelect.value === "Inches";
  const steps_mm = inches
    // 1/8 in = 3.175, 1/4 = 6.35, 1/2 = 12.7, 1 in = 25.4, etc.
    ? [3.175, 6.35, 12.7, 25.4, 50.8, 127.0, 254.0, 508.0, 1270.0, 2540.0]
    : [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000];
  for (const s of steps_mm) {
    if (s * scale >= minPxBetweenMajorTicks) return s;
  }
  return steps_mm[steps_mm.length - 1];
}

/**
 * Format a puzzle-space coordinate (in mm) for display on the ruler,
 * in the currently selected display unit.
 */
function formatRulerLabel(mm: number): string {
  if (unitSelect.value === "Inches") {
    const inches = mm / 25.4;
    const rounded = Math.round(inches * 100) / 100;
    return Number.isInteger(rounded) ? rounded.toFixed(0) : rounded.toFixed(2);
  }
  const rounded = Math.round(mm * 100) / 100;
  return Number.isInteger(rounded) ? rounded.toFixed(0) : String(rounded);
}

/**
 * Draw ticked rulers on the horizontal + vertical canvases that sit
 * along the top and left of the puzzle viewport. The rulers use the
 * same `scale` and `panX`/`panY` as the puzzle canvas, so ticks line
 * up with the underlying puzzle geometry at any zoom/pan.
 */
function drawRulers(scale: number): void {
  if (!rulerHCtx || !rulerVCtx) return;

  const tickShort = 4;
  const tickMid = 8;
  const tickLong = 12;
  const labelFont = "10px system-ui, -apple-system, sans-serif";
  const tickColor = "#999";
  const labelColor = "#555";

  // Horizontal ruler.
  {
    const c = rulerHCtx;
    const w = rulerHCanvas.clientWidth;
    const h = rulerHCanvas.clientHeight;
    c.clearRect(0, 0, w, h);
    c.fillStyle = labelColor;
    c.strokeStyle = tickColor;
    c.lineWidth = 1;
    c.font = labelFont;
    c.textBaseline = "top";
    c.textAlign = "center";

    const majorStep = pickMajorStep(scale, 50);
    const minorStep = majorStep / 5;
    const leftMm = -panX / scale;
    const rightMm = leftMm + w / scale;

    // Minor ticks
    const firstMinor = Math.ceil(leftMm / minorStep) * minorStep;
    c.beginPath();
    for (let mm = firstMinor; mm <= rightMm + 1e-6; mm += minorStep) {
      const x = Math.round(panX + mm * scale) + 0.5;
      // Skip if it coincides with a major tick (drawn below).
      const nearMajor =
        Math.abs(mm / majorStep - Math.round(mm / majorStep)) < 1e-6;
      if (nearMajor) continue;
      c.moveTo(x, h);
      c.lineTo(x, h - tickShort);
    }
    c.stroke();

    // Major ticks + labels
    const firstMajor = Math.ceil(leftMm / majorStep) * majorStep;
    c.beginPath();
    for (let mm = firstMajor; mm <= rightMm + 1e-6; mm += majorStep) {
      const x = Math.round(panX + mm * scale) + 0.5;
      c.moveTo(x, h);
      c.lineTo(x, h - tickLong);
    }
    c.stroke();
    for (let mm = firstMajor; mm <= rightMm + 1e-6; mm += majorStep) {
      const x = Math.round(panX + mm * scale);
      c.fillText(formatRulerLabel(mm), x, 2);
    }

    // Mid subdivisions (thicker than minor, thinner than major)
    c.strokeStyle = tickColor;
    c.beginPath();
    for (let mm = firstMinor; mm <= rightMm + 1e-6; mm += minorStep) {
      // Mid-tick at halfway between major ticks (2.5 minor from major).
      const halfRatio = mm / (majorStep / 2);
      const nearHalf = Math.abs(halfRatio - Math.round(halfRatio)) < 1e-6;
      const nearMajor =
        Math.abs(mm / majorStep - Math.round(mm / majorStep)) < 1e-6;
      if (nearHalf && !nearMajor) {
        const x = Math.round(panX + mm * scale) + 0.5;
        c.moveTo(x, h);
        c.lineTo(x, h - tickMid);
      }
    }
    c.stroke();
  }

  // Vertical ruler (same logic, rotated).
  {
    const c = rulerVCtx;
    const w = rulerVCanvas.clientWidth;
    const h = rulerVCanvas.clientHeight;
    c.clearRect(0, 0, w, h);
    c.fillStyle = labelColor;
    c.strokeStyle = tickColor;
    c.lineWidth = 1;
    c.font = labelFont;
    c.textBaseline = "middle";
    c.textAlign = "center";

    const majorStep = pickMajorStep(scale, 50);
    const minorStep = majorStep / 5;
    const topMm = -panY / scale;
    const bottomMm = topMm + h / scale;

    const firstMinor = Math.ceil(topMm / minorStep) * minorStep;
    c.beginPath();
    for (let mm = firstMinor; mm <= bottomMm + 1e-6; mm += minorStep) {
      const y = Math.round(panY + mm * scale) + 0.5;
      const nearMajor =
        Math.abs(mm / majorStep - Math.round(mm / majorStep)) < 1e-6;
      if (nearMajor) continue;
      c.moveTo(w, y);
      c.lineTo(w - tickShort, y);
    }
    c.stroke();

    const firstMajor = Math.ceil(topMm / majorStep) * majorStep;
    c.beginPath();
    for (let mm = firstMajor; mm <= bottomMm + 1e-6; mm += majorStep) {
      const y = Math.round(panY + mm * scale) + 0.5;
      c.moveTo(w, y);
      c.lineTo(w - tickLong, y);
    }
    c.stroke();
    // Labels: rotated 90° so they run along the ruler.
    for (let mm = firstMajor; mm <= bottomMm + 1e-6; mm += majorStep) {
      const y = Math.round(panY + mm * scale);
      c.save();
      c.translate(w / 2 - 2, y);
      c.rotate(-Math.PI / 2);
      c.fillText(formatRulerLabel(mm), 0, 0);
      c.restore();
    }

    c.beginPath();
    for (let mm = firstMinor; mm <= bottomMm + 1e-6; mm += minorStep) {
      const halfRatio = mm / (majorStep / 2);
      const nearHalf = Math.abs(halfRatio - Math.round(halfRatio)) < 1e-6;
      const nearMajor =
        Math.abs(mm / majorStep - Math.round(mm / majorStep)) < 1e-6;
      if (nearHalf && !nearMajor) {
        const y = Math.round(panY + mm * scale) + 0.5;
        c.moveTo(w, y);
        c.lineTo(w - tickMid, y);
      }
    }
    c.stroke();
  }
}

// ─── Canvas Drawing ──────────────────────────────────────────

/**
 * Parse a command-prefixed Float64Array and issue canvas drawing calls.
 * Format (matches the Rust CMD_ constants in binary_export.rs):
 *   0 (moveTo):   +2 floats (x, y)
 *   1 (lineTo):   +2 floats (x, y)
 *   2 (curveTo):  +6 floats (cp1x, cp1y, cp2x, cp2y, x, y)
 *   3 (closePath): +0 floats
 * Caller is responsible for beginPath/stroke around this.
 */
/**
 * Walk the command-prefixed binary stream directly into a context.
 * Used for one-off paths drawn under per-frame transforms (e.g. the
 * whimsy ghost overlay) where caching as a Path2D doesn't help. For
 * static-per-generation paths (edges, border) prefer `binaryToPath2D`
 * + `ctx.stroke(path)`, which avoids re-walking the stream every frame.
 */
function playCommands(c: CanvasRenderingContext2D, data: Float64Array): void {
  let i = 0;
  const len = data.length;
  while (i < len) {
    const cmd = data[i];
    if (cmd === 0) {
      c.moveTo(data[i + 1], data[i + 2]);
      i += 3;
    } else if (cmd === 1) {
      c.lineTo(data[i + 1], data[i + 2]);
      i += 3;
    } else if (cmd === 2) {
      c.bezierCurveTo(
        data[i + 1],
        data[i + 2],
        data[i + 3],
        data[i + 4],
        data[i + 5],
        data[i + 6],
      );
      i += 7;
    } else if (cmd === 3) {
      c.closePath();
      i += 1;
    } else {
      i += 1;
    }
  }
}

/**
 * Walk the command-prefixed binary stream into a fresh `Path2D`.
 * Commands match `crates/puzzle-core/src/binary_export.rs`:
 *   0 = moveTo (x, y)
 *   1 = lineTo (x, y)
 *   2 = bezierCurveTo (cp1x, cp1y, cp2x, cp2y, x, y)
 *   3 = closePath
 */
function binaryToPath2D(data: Float64Array): Path2D {
  const path = new Path2D();
  let i = 0;
  const len = data.length;
  while (i < len) {
    const cmd = data[i];
    if (cmd === 0) {
      path.moveTo(data[i + 1], data[i + 2]);
      i += 3;
    } else if (cmd === 1) {
      path.lineTo(data[i + 1], data[i + 2]);
      i += 3;
    } else if (cmd === 2) {
      path.bezierCurveTo(
        data[i + 1],
        data[i + 2],
        data[i + 3],
        data[i + 4],
        data[i + 5],
        data[i + 6],
      );
      i += 7;
    } else if (cmd === 3) {
      path.closePath();
      i += 1;
    } else {
      i += 1;
    }
  }
  return path;
}

function drawBorder(c: CanvasRenderingContext2D): void {
  if (!borderPath2D) return;
  c.stroke(borderPath2D);
}

function drawEdges(c: CanvasRenderingContext2D): void {
  if (!edgesPath2D) return;
  c.stroke(edgesPath2D);
}

function drawPuzzle(): void {
  if (!ctx || !edgesPath2D || !borderPath2D) return;

  const vpW = svgViewport.clientWidth;
  const vpH = svgViewport.clientHeight;

  // Clear
  ctx.clearRect(0, 0, vpW, vpH);

  // Compute transform: puzzle mm coords -> screen pixels
  const baseScale = vpW / puzzleWidth;
  const scale = baseScale * zoomLevel;

  // Set up canvas transform
  ctx.save();
  ctx.translate(panX, panY);
  ctx.scale(scale, scale);

  // Style
  ctx.strokeStyle = "#000000";
  ctx.lineWidth = Math.max(1, 0.2 * scale) / scale;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";

  drawBorder(ctx);
  drawEdges(ctx);

  ctx.restore();

  drawSeedDots(ctx);
  drawSelectionOverlay(ctx);
  drawRulers(scale);
}

/**
 * Debug overlay: plot current piece centers (solid red dots) and the
 * initial anchor positions the CVT started from (hollow red rings).
 * Comparing the two shows how far Lloyd relaxation moved each seed.
 */
function drawSeedDots(c: CanvasRenderingContext2D): void {
  if (!seedsVisibleCheckbox || !seedsVisibleCheckbox.checked) return;
  const scale = currentScale();
  const toScreen = (mx: number, my: number) => ({
    sx: panX + mx * scale,
    sy: panY + my * scale,
  });

  c.save();

  // Initial anchor positions (hollow rings — "where they were")
  if (anchorsData && anchorsData.length >= 2) {
    c.strokeStyle = "#d93025";
    c.lineWidth = 1.5;
    for (let i = 0; i + 1 < anchorsData.length; i += 2) {
      const { sx, sy } = toScreen(anchorsData[i], anchorsData[i + 1]);
      c.beginPath();
      c.arc(sx, sy, 5, 0, 2 * Math.PI);
      c.stroke();
    }
  }

  // Current piece centers (solid dots — "where they are")
  if (centersData && centersData.length >= 2) {
    c.fillStyle = "#d93025";
    for (let i = 0; i + 1 < centersData.length; i += 2) {
      const { sx, sy } = toScreen(centersData[i], centersData[i + 1]);
      c.beginPath();
      c.arc(sx, sy, 3, 0, 2 * Math.PI);
      c.fill();
    }
  }

  c.restore();
}

// ─── Canvas ↔ Whimsy Hit-Testing Helpers ────────────────────

/**
 * Current scale factor: puzzle mm → screen pixels. Matches the
 * transform set up inside `drawPuzzle`.
 */
function currentScale(): number {
  if (puzzleWidth <= 0 || svgViewport.clientWidth <= 0) return 1;
  return (svgViewport.clientWidth / puzzleWidth) * zoomLevel;
}

function cursorToMm(e: { clientX: number; clientY: number }): {
  x: number;
  y: number;
} {
  const rect = svgViewport.getBoundingClientRect();
  const s = currentScale();
  return {
    x: (e.clientX - rect.left - panX) / s,
    y: (e.clientY - rect.top - panY) / s,
  };
}

/** Rotate (px, py) around (cx, cy) by `angleRad` (screen-space CCW when viewed normally — matches canvas Y-down convention). */
function rotatePt(
  px: number,
  py: number,
  cx: number,
  cy: number,
  angleRad: number,
): { x: number; y: number } {
  const dx = px - cx;
  const dy = py - cy;
  const c = Math.cos(angleRad);
  const s = Math.sin(angleRad);
  return { x: cx + dx * c - dy * s, y: cy + dx * s + dy * c };
}

/** Is `(mmX, mmY)` inside the whimsy's rotated square bbox? */
function whimsyContains(w: WhimsyInstance, mmX: number, mmY: number): boolean {
  const rad = (-w.rotation * Math.PI) / 180;
  const local = rotatePt(mmX, mmY, w.centerX, w.centerY, rad);
  const half = w.size / 2;
  return (
    local.x >= w.centerX - half &&
    local.x <= w.centerX + half &&
    local.y >= w.centerY - half &&
    local.y <= w.centerY + half
  );
}

/** Inverse-rotate a mm point into the whimsy's local (un-rotated) frame, then translate center to origin. */
function whimsyLocal(w: WhimsyInstance, mmX: number, mmY: number): { x: number; y: number } {
  const rad = (-w.rotation * Math.PI) / 180;
  const p = rotatePt(mmX, mmY, w.centerX, w.centerY, rad);
  return { x: p.x - w.centerX, y: p.y - w.centerY };
}

/**
 * Compute the five manipulation handle positions for the given whimsy,
 * in CSS-pixel screen coordinates. Four corner handles for uniform
 * scale + one rotation handle above the top-middle. The rotation
 * handle offset is in mm (scales with zoom) so it stays anchored to
 * the whimsy rather than drifting away as you zoom out.
 */
function whimsyScreenHandles(w: WhimsyInstance): {
  corners: Array<{ sx: number; sy: number; dir: CornerDir }>;
  topMid: { sx: number; sy: number };
  rotate: { sx: number; sy: number };
} {
  const s = currentScale();
  const rad = (w.rotation * Math.PI) / 180;
  const half = w.size / 2;
  const mmToScreen = (mx: number, my: number) => ({
    sx: panX + mx * s,
    sy: panY + my * s,
  });

  const corners: Array<{ sx: number; sy: number; dir: CornerDir }> = [
    { dx: -half, dy: -half, dir: "tl" as CornerDir },
    { dx: +half, dy: -half, dir: "tr" as CornerDir },
    { dx: +half, dy: +half, dir: "br" as CornerDir },
    { dx: -half, dy: +half, dir: "bl" as CornerDir },
  ].map((c) => {
    const p = rotatePt(
      w.centerX + c.dx,
      w.centerY + c.dy,
      w.centerX,
      w.centerY,
      rad,
    );
    return { ...mmToScreen(p.x, p.y), dir: c.dir };
  });

  const topMidMm = rotatePt(
    w.centerX,
    w.centerY - half,
    w.centerX,
    w.centerY,
    rad,
  );
  const rotateOffsetMm = w.size * 0.125 + 7.5 / s;
  const rotateMm = rotatePt(
    w.centerX,
    w.centerY - half - rotateOffsetMm,
    w.centerX,
    w.centerY,
    rad,
  );

  return {
    corners,
    topMid: mmToScreen(topMidMm.x, topMidMm.y),
    rotate: mmToScreen(rotateMm.x, rotateMm.y),
  };
}

type HandleHit = CornerDir | "rotate" | null;
function handleHitTest(w: WhimsyInstance, sx: number, sy: number): HandleHit {
  const h = whimsyScreenHandles(w);
  for (const c of h.corners) {
    if (Math.hypot(sx - c.sx, sy - c.sy) <= HANDLE_HIT_RADIUS_PX) return c.dir;
  }
  if (Math.hypot(sx - h.rotate.sx, sy - h.rotate.sy) <= HANDLE_HIT_RADIUS_PX)
    return "rotate";
  return null;
}

function cursorForCorner(dir: CornerDir): string {
  return dir === "tl" || dir === "br" ? "nwse-resize" : "nesw-resize";
}

function findWhimsyAt(mmX: number, mmY: number): WhimsyInstance | null {
  if (isClassicGrid()) return null;
  // Iterate in reverse so the most-recently-added whimsy takes
  // priority on overlap — matches the user's "top" mental model.
  for (let i = whimsies.length - 1; i >= 0; i--) {
    if (whimsyContains(whimsies[i], mmX, mmY)) return whimsies[i];
  }
  return null;
}

// ─── Selection Overlay ──────────────────────────────────────

function drawSelectionOverlay(c: CanvasRenderingContext2D): void {
  if (isClassicGrid() || selectedWhimsyId === null) return;
  const w = whimsies.find((x) => x.id === selectedWhimsyId);
  if (!w) return;

  const h = whimsyScreenHandles(w);
  const scale = currentScale();

  c.save();
  c.lineCap = "round";
  c.lineJoin = "round";

  // Ghost of the whimsy shape at its current transform. The cached
  // path lives in a 1 × 1 box with top-left at (0, 0); translate the
  // center into screen coords, rotate, scale to whimsy size × zoom,
  // then re-center the unit box so 0.5 sits at the whimsy center.
  const unitPath = getShapeUnitPath(w.shape);
  if (unitPath !== undefined && unitPath.length > 0) {
    c.save();
    c.translate(panX + w.centerX * scale, panY + w.centerY * scale);
    c.rotate((w.rotation * Math.PI) / 180);
    c.scale(w.size * scale, w.size * scale);
    c.translate(-0.5, -0.5);
    c.beginPath();
    playCommands(c, unitPath);
    c.restore();
    c.fillStyle = "rgba(74, 144, 217, 0.12)";
    c.strokeStyle = "rgba(74, 144, 217, 0.8)";
    c.lineWidth = 1.5;
    c.fill();
    c.stroke();
  }

  // Dashed rotated bbox
  c.strokeStyle = "#4a90d9";
  c.lineWidth = 1.5;
  c.setLineDash([5, 4]);
  c.beginPath();
  c.moveTo(h.corners[0].sx, h.corners[0].sy);
  for (let i = 1; i < 4; i++) c.lineTo(h.corners[i].sx, h.corners[i].sy);
  c.closePath();
  c.stroke();
  c.setLineDash([]);

  // Line from top-middle to rotation handle
  c.beginPath();
  c.moveTo(h.topMid.sx, h.topMid.sy);
  c.lineTo(h.rotate.sx, h.rotate.sy);
  c.stroke();

  // Corner handles (filled squares)
  c.fillStyle = "#ffffff";
  for (const cr of h.corners) {
    c.beginPath();
    c.rect(
      cr.sx - HANDLE_SIZE_PX / 2,
      cr.sy - HANDLE_SIZE_PX / 2,
      HANDLE_SIZE_PX,
      HANDLE_SIZE_PX,
    );
    c.fill();
    c.stroke();
  }

  // Rotation handle (filled circle)
  c.beginPath();
  c.arc(h.rotate.sx, h.rotate.sy, HANDLE_SIZE_PX / 2 + 1, 0, 2 * Math.PI);
  c.fill();
  c.stroke();

  c.restore();
}

// ─── Zoom/Pan Helpers ───────────────────────────────────────

function applyTransform(): void {
  drawPuzzle();
  zoomLevelDisplay.textContent = `${Math.round(zoomLevel * 100)}%`;
}

function resetZoom(): void {
  zoomLevel = 1;
  panX = 0;
  panY = 0;
  if (canvas && puzzleWidth > 0) {
    const vpH = svgViewport.clientHeight;
    const baseScale = svgViewport.clientWidth / puzzleWidth;
    const svgH = puzzleHeight * baseScale;
    panY = Math.max(0, (vpH - svgH) / 2);
  }
  applyTransform();
}

// ─── rAF-Throttled Transform ─────────────────────────────────

let transformRafPending = false;
function scheduleTransform(): void {
  if (transformRafPending) return;
  transformRafPending = true;
  requestAnimationFrame(() => {
    transformRafPending = false;
    applyTransform();
  });
}

// ─── Throttled Generation ────────────────────────────────────

function scheduleGenerate(): void {
  if (rafPending) return;
  rafPending = true;
  requestAnimationFrame(() => {
    rafPending = false;
    generatePuzzle();
  });
}

// ─── Puzzle Generation ───────────────────────────────────────

async function generatePuzzle(): Promise<void> {
  const config = buildConfig();
  const configJson = JSON.stringify(config);

  // Worker performs the WASM build off-main-thread. `requestBuild`
  // coalesces — if multiple builds are queued in quick succession,
  // only the latest one's result reaches us; older ones reject with
  // BUILD_SUPERSEDED, which we silently swallow.
  let result;
  try {
    result = await requestBuild(configJson);
  } catch (err) {
    if (err === BUILD_SUPERSEDED) {
      // Newer build is already in flight — drop this stale result.
      return;
    }
    errorDisplay.textContent =
      err instanceof Error ? err.message : "Generation failed";
    errorDisplay.style.display = "block";
    return;
  }

  edgesData = result.edges ?? null;
  borderData = result.border ?? null;
  centersData = result.centers ?? null;
  anchorsData = result.anchors ?? null;
  puzzleWidth = result.width;
  puzzleHeight = result.height;
  edgesPath2D = edgesData ? binaryToPath2D(edgesData) : null;
  borderPath2D = borderData ? binaryToPath2D(borderData) : null;

  errorDisplay.style.display = "none";

  // Use WASM-returned piece count (accurate for boundary puzzles)
  const count = result.piece_count as number | undefined;
  generatedPieceCount = count ?? 0;
  const borderVal = borderShapeSelect.value;
  if (count != null) {
    const suffixes: string[] = [];
    if (result.grid_columns && result.grid_rows) {
      suffixes.push(`${result.grid_columns} × ${result.grid_rows} classic grid`);
      if (count !== parseInt(pieceTargetInput.value, 10)) {
        suffixes.push(`target ${pieceTargetInput.value}`);
      }
    }
    if (borderVal && borderVal !== "rectangle") {
      suffixes.push(`${borderVal} border`);
    }
    if (!isClassicGrid() && whimsies.length > 0) {
      const typed = parseInt(pieceTargetInput.value, 10) || 0;
      const fromWhimsies = Math.max(0, count - typed);
      suffixes.push(
        `${typed} + ${fromWhimsies} from ${whimsies.length} whims${whimsies.length === 1 ? "y" : "ies"}`,
      );
    }
    pieceCount.textContent = suffixes.length
      ? `${count} pieces (${suffixes.join(", ")})`
      : `${count} pieces`;
  } else {
    pieceCount.textContent = `${pieceTargetInput.value} pieces`;
  }

  // Resize canvas and draw
  resizeCanvas();
  drawPuzzle();

  // Update URL with current params (debounced)
  scheduleURLUpdate();
}

// ─── Readout Updaters ───────────────────────────────────────
//
// Currently a no-op — tab shape is fixed in the Rust layer. Kept as an
// extension point for any future visible-value controls.
function updateReadouts(): void {}

// ─── Randomize Toggle Helpers ────────────────────────────────

function toggleRandomize(
  checkbox: HTMLInputElement,
  maxSlider: HTMLInputElement,
  minSlider: HTMLInputElement,
): void {
  const pill = checkbox.closest('.pill-toggle');
  if (pill) pill.classList.toggle('active', checkbox.checked);
  if (checkbox.checked) {
    maxSlider.style.display = "";
    // Center-aware knob placement
    const currentValue = parseFloat(minSlider.value);
    const sliderMin = parseFloat(minSlider.min);
    const sliderMax = parseFloat(minSlider.max);
    const midpoint = (sliderMin + sliderMax) / 2;
    if (currentValue < midpoint) {
      // Left of center: keep value as min, max knob goes to slider maximum
      maxSlider.value = String(sliderMax);
    } else {
      // Right of center (or at midpoint): value becomes max, min knob goes to slider minimum
      maxSlider.value = String(currentValue);
      minSlider.value = String(sliderMin);
    }
  } else {
    maxSlider.style.display = "none";
  }
  updateReadouts();
  scheduleGenerate();
}

function clampMinMax(
  minSlider: HTMLInputElement,
  maxSlider: HTMLInputElement,
): void {
  const step = parseFloat(minSlider.step) || 0.01;
  const max = parseFloat(maxSlider.max);
  // Don't let min get so high that max can't stay one step above it
  const minCeiling = max - step;
  if (parseFloat(minSlider.value) > minCeiling) {
    minSlider.value = String(minCeiling);
  }
  if (parseFloat(maxSlider.value) <= parseFloat(minSlider.value)) {
    maxSlider.value = String(parseFloat(minSlider.value) + step);
  }
}

function clampMaxMin(
  minSlider: HTMLInputElement,
  maxSlider: HTMLInputElement,
): void {
  const step = parseFloat(minSlider.step) || 0.01;
  const min = parseFloat(minSlider.min);
  // Don't let max get so low that min can't stay one step below it
  const maxFloor = min + step;
  if (parseFloat(maxSlider.value) < maxFloor) {
    maxSlider.value = String(maxFloor);
  }
  if (parseFloat(minSlider.value) >= parseFloat(maxSlider.value)) {
    minSlider.value = String(parseFloat(maxSlider.value) - step);
  }
}

// ─── Unit Conversion ─────────────────────────────────────────

function convertDimensions(oldUnit: string, newUnit: string): void {
  if (oldUnit === newUnit) return;
  const factor = newUnit === "Inches" ? 1 / 25.4 : 25.4;
  const w = parseFloat(widthInput.value);
  const h = parseFloat(heightInput.value);
  if (!isNaN(w)) {
    widthInput.value = parseFloat((w * factor).toFixed(2)).toString();
  }
  if (!isNaN(h)) {
    heightInput.value = parseFloat((h * factor).toFixed(2)).toString();
  }
}

function toggleLock(checkbox: HTMLInputElement, label: string): boolean {
  const active = checkbox.checked;
  const pill = checkbox.closest('.pill-toggle')!;
  pill.classList.toggle('active', active);
  // Swap lock shackle between open (unlocked) and closed (locked)
  const shackle = pill.querySelector('.lock-shackle') as SVGPathElement | null;
  if (shackle) {
    shackle.setAttribute('d', active
      ? 'M5 7V5a3 3 0 0 1 6 0v2'   // closed shackle
      : 'M5 7V5a3 3 0 0 1 6 0');    // open shackle
  }
  pill.setAttribute('title', active ? `Unlock ${label}` : `Lock ${label}`);
  return active;
}

function showWarnings(warnings: string[]): void {
  pieceSizeWarning.innerHTML = warnings.map((w) => `<li>${w}</li>`).join("");
}

/**
 * Advisory check: warn when the current W×H × shape combination
 * can't fit the requested piece count without knobs falling below the
 * ~3 mm neck-opening threshold. Never mutates inputs — the user is in
 * charge of the dimensions, we just surface when the geometry will
 * produce some straight-line (no-knob) edges instead of full knobs.
 *
 * Required area derivation:
 *   required_area = piece_count × MIN_CELL_DIM² × shape_factor²
 * where `shape_factor` accounts for hex-vs-square cells and (for
 * shapes) the shape's bbox fill ratio.
 */
function enforceConstraints(): void {
  const pc = parseInt(pieceTargetInput.value, 10);
  const w = parseFloat(widthInput.value);
  const h = parseFloat(heightInput.value);
  if (isNaN(pc) || pc < 2 || isNaN(w) || isNaN(h) || w <= 0 || h <= 0) {
    pieceSizeWarning.innerHTML = "";
    return;
  }

  const factor = unitSelect.value === "Inches" ? 25.4 : 1;
  const currentArea = w * factor * h * factor;
  const mult = shapeDimMultiplier();
  const requiredArea = pc * MIN_CELL_DIM_MM * MIN_CELL_DIM_MM * mult * mult;

  const warnings: string[] = [];
  if (currentArea < requiredArea) {
    const unitLabel = unitSelect.value === "Inches" ? "in" : "mm";
    const scale = Math.sqrt(requiredArea / currentArea);
    const suggestedW = Math.round((w * scale) * 10) / 10;
    const suggestedH = Math.round((h * scale) * 10) / 10;
    warnings.push(
      `Dimensions are small for ${pc} pieces — some knobs will be thinner than 3mm. Try ≥ ${suggestedW} × ${suggestedH} ${unitLabel} for clean knobs.`,
    );
  }
  showWarnings(warnings);
}

// ─── Main ───────────────────────────────────────────────────

async function main(): Promise<void> {
  const loadingEl = document.getElementById("loading")!;
  const appEl = document.getElementById("app")!;

  try {
    // WASM is now loaded inside the regen worker (`worker-client.ts`).
    // No init step needed on the main thread — `requestBuild` is
    // available immediately and the worker handles its own init
    // before processing the first message.
    loadingEl.style.display = "none";
    appEl.style.display = "block";
  } catch (err) {
    loadingEl.textContent = `Failed to load app: ${err}`;
    console.error("App init failed:", err);
    return;
  }

  // Cache DOM references
  widthInput = document.getElementById("width") as HTMLInputElement;
  heightInput = document.getElementById("height") as HTMLInputElement;
  unitSelect = document.getElementById("unit") as HTMLSelectElement;
  seedInput = document.getElementById("seed") as HTMLInputElement;
  pieceCount = document.getElementById("piece-count")!;
  errorDisplay = document.getElementById("error-display")!;

  borderShapeSelect = document.getElementById("border-shape") as HTMLSelectElement;
  cellAlgorithmSelect = document.getElementById(
    "cell-algorithm",
  ) as HTMLSelectElement;
  poissonPolishSelect = document.getElementById(
    "poisson-polish",
  ) as HTMLSelectElement;
  poissonPolishGroup = document.getElementById("poisson-polish-group")!;
  classicKnobSizeInput = document.getElementById("classic-knob-size") as HTMLInputElement;
  classicGridIrregularityInput = document.getElementById("classic-grid-irregularity") as HTMLInputElement;
  classicKnobRandomnessInput = document.getElementById("classic-knob-randomness") as HTMLInputElement;

  pieceTargetInput = document.getElementById("piece-target") as HTMLInputElement;
  pieceSizeWarning = document.getElementById("piece-size-warning")!;
  dimsLockCheckbox = document.getElementById("dims-lock") as HTMLInputElement;
  knobsEnabledCheckbox = document.getElementById("knobs-enabled") as HTMLInputElement;
  edgeKnobsEnabledCheckbox = document.getElementById("edge-knobs-enabled") as HTMLInputElement;
  seedsVisibleCheckbox = document.getElementById("seeds-visible") as HTMLInputElement;

  whimsyList = document.getElementById("whimsy-list") as HTMLUListElement;
  addWhimsyBtn = document.getElementById("add-whimsy") as HTMLButtonElement;
  shapePicker = document.getElementById("shape-picker") as HTMLDialogElement;

  rulerHCanvas = document.getElementById("ruler-h") as HTMLCanvasElement;
  rulerVCanvas = document.getElementById("ruler-v") as HTMLCanvasElement;
  rulerHCtx = rulerHCanvas.getContext("2d");
  rulerVCtx = rulerVCanvas.getContext("2d");
  svgViewport = document.getElementById("svg-viewport")!;
  zoomLevelDisplay = document.getElementById("zoom-level")!;
  zoomInBtn = document.getElementById("zoom-in")!;
  zoomOutBtn = document.getElementById("zoom-out")!;
  zoomResetBtn = document.getElementById("zoom-reset")!;

  // Initialize canvas
  canvas = document.getElementById("puzzle-canvas") as HTMLCanvasElement;
  ctx = canvas.getContext("2d");

  // ResizeObserver for viewport resize
  const resizeObserver = new ResizeObserver(() => {
    resizeCanvas();
    drawPuzzle();
  });
  resizeObserver.observe(svgViewport);

  // Load params from URL (if any), otherwise generate random seed
  const hasUrlParams = loadFromURL();
  if (!hasUrlParams) {
    seedInput.value = randomSeed();
  }
  // Reflect any whimsies restored from the URL into the sidebar.
  renderWhimsies();

  updateReadouts();

  // Track previous unit for dimension conversion on unit change
  let previousUnit = unitSelect.value;

  // ─── Event Wiring ───────────────────────────────────────

  dimsLockCheckbox.addEventListener("change", () => {
    dimsLocked = toggleLock(dimsLockCheckbox, "dimensions");
  });

  // Reflect the default-checked state into the pill's `.active` styling.
  knobsEnabledCheckbox
    .closest(".pill-toggle")
    ?.classList.toggle("active", knobsEnabledCheckbox.checked);
  knobsEnabledCheckbox.addEventListener("change", () => {
    knobsEnabledCheckbox
      .closest(".pill-toggle")
      ?.classList.toggle("active", knobsEnabledCheckbox.checked);
    syncClassicStyleControls();
    scheduleGenerate();
  });

  // Edge-piece knobs toggle: regenerates the puzzle so knobs near
  // the boundary appear/disappear immediately.
  edgeKnobsEnabledCheckbox
    .closest(".pill-toggle")
    ?.classList.toggle("active", edgeKnobsEnabledCheckbox.checked);
  edgeKnobsEnabledCheckbox.addEventListener("change", () => {
    edgeKnobsEnabledCheckbox
      .closest(".pill-toggle")
      ?.classList.toggle("active", edgeKnobsEnabledCheckbox.checked);
    scheduleGenerate();
  });

  // Seeds toggle: redraw only, no regen needed (data already returned
  // from WASM unconditionally).
  seedsVisibleCheckbox
    .closest(".pill-toggle")
    ?.classList.toggle("active", seedsVisibleCheckbox.checked);
  seedsVisibleCheckbox.addEventListener("change", () => {
    seedsVisibleCheckbox
      .closest(".pill-toggle")
      ?.classList.toggle("active", seedsVisibleCheckbox.checked);
    applyTransform();
  });

  // Piece count
  pieceTargetInput.addEventListener("input", () => {
    enforceConstraints();
    scheduleGenerate();
  });

  // Dimension inputs
  for (const input of [widthInput, heightInput]) {
    input.addEventListener("input", () => {
      enforceConstraints();
      scheduleGenerate();
    });
  }

  // Unit select — convert dimensions
  unitSelect.addEventListener("change", () => {
    const newUnit = unitSelect.value;
    convertDimensions(previousUnit, newUnit);
    previousUnit = newUnit;
    generatePuzzle();
    enforceConstraints();
  });

  // Border shape select — required area depends on it, re-run
  // constraints so dimensions auto-grow the moment it changes.
  borderShapeSelect.addEventListener("change", () => {
    enforceConstraints();
    scheduleGenerate();
  });

  // Cell-generation algorithm select — switching algorithm is a
  // structural change to the layout; trigger an immediate full
  // regen and toggle the polish-iterations control's visibility
  // (it's only meaningful for Poisson).
  cellAlgorithmSelect.addEventListener("change", () => {
    syncAlgorithmControls();
    enforceConstraints();
    scheduleGenerate();
  });

  // Polish iteration count — Poisson-only. Trigger a full regen on
  // change so the user sees the effect immediately.
  poissonPolishSelect.addEventListener("change", () => {
    scheduleGenerate();
  });

  for (const { input } of classicStyleControls()) {
    input.addEventListener("input", () => {
      syncClassicStyleControls();
      enforceConstraints();
      scheduleGenerate();
    });
  }
  syncClassicStyleControls();

  // Set initial visibility based on whatever URL/persisted state
  // chose at load time.
  syncAlgorithmControls();

  // Whimsies: open the shape picker; on close, add the selected shape.
  addWhimsyBtn.addEventListener("click", () => {
    shapePicker.showModal();
  });
  shapePicker.addEventListener("close", () => {
    const value = shapePicker.returnValue;
    if (value && value !== "cancel") {
      addWhimsy(value);
    }
    shapePicker.returnValue = "";
  });

  // Seed text input
  seedInput.addEventListener("input", scheduleGenerate);

  // Randomize button
  const randomizeBtn = document.getElementById("randomize")!;
  randomizeBtn.addEventListener("click", () => {
    seedInput.value = randomSeed();
    scheduleGenerate();
  });

  // ─── Zoom/Pan Event Wiring ──────────────────────────────

  // Wheel zoom — zoom toward cursor position
  svgViewport.addEventListener(
    "wheel",
    (e: WheelEvent) => {
      e.preventDefault();
      const rect = svgViewport.getBoundingClientRect();
      const mouseX = e.clientX - rect.left;
      const mouseY = e.clientY - rect.top;

      const oldZoom = zoomLevel;
      if (e.deltaY < 0) {
        zoomLevel = Math.min(MAX_ZOOM, zoomLevel * ZOOM_STEP);
      } else {
        zoomLevel = Math.max(MIN_ZOOM, zoomLevel / ZOOM_STEP);
      }

      // Adjust pan so zoom centers on cursor
      const zoomRatio = zoomLevel / oldZoom;
      panX = mouseX - zoomRatio * (mouseX - panX);
      panY = mouseY - zoomRatio * (mouseY - panY);

      scheduleTransform();
    },
    { passive: false },
  );

  // Mouse: routed by interaction mode — handle drag → scale/rotate
  // whimsy, body drag → move whimsy, empty → pan and deselect.
  svgViewport.addEventListener("mousedown", (e: MouseEvent) => {
    if (e.button !== 0) return; // left click only
    const rect = svgViewport.getBoundingClientRect();
    const sx = e.clientX - rect.left;
    const sy = e.clientY - rect.top;
    const mm = cursorToMm(e);

    // Priority 1: a handle on the currently-selected whimsy.
    if (selectedWhimsyId !== null) {
      const sel = whimsies.find((w) => w.id === selectedWhimsyId);
      if (sel) {
        const hit = handleHitTest(sel, sx, sy);
        if (hit === "rotate") {
          const angDeg =
            (Math.atan2(mm.y - sel.centerY, mm.x - sel.centerX) * 180) /
            Math.PI;
          interaction = {
            kind: "rotating-whimsy",
            id: sel.id,
            initialRotation: sel.rotation,
            initialAngleDeg: angDeg,
            committed: false,
          };
          e.preventDefault();
          return;
        } else if (hit !== null) {
          const loc = whimsyLocal(sel, mm.x, mm.y);
          const initialDist = Math.max(Math.abs(loc.x), Math.abs(loc.y));
          interaction = {
            kind: "scaling-whimsy",
            id: sel.id,
            corner: hit,
            initialSize: sel.size,
            initialDist: Math.max(1, initialDist),
            committed: false,
          };
          e.preventDefault();
          return;
        }
      }
    }

    // Priority 2: click inside any whimsy → select + begin drag.
    const hitWhimsy = findWhimsyAt(mm.x, mm.y);
    if (hitWhimsy) {
      selectedWhimsyId = hitWhimsy.id;
      interaction = {
        kind: "dragging-whimsy",
        id: hitWhimsy.id,
        offsetX: hitWhimsy.centerX - mm.x,
        offsetY: hitWhimsy.centerY - mm.y,
        startCenterX: hitWhimsy.centerX,
        startCenterY: hitWhimsy.centerY,
        committed: false,
      };
      applyTransform();
      e.preventDefault();
      return;
    }

    // Priority 3: click empty canvas → deselect (if selected) and pan.
    if (selectedWhimsyId !== null) {
      selectedWhimsyId = null;
      applyTransform();
    }
    interaction = {
      kind: "panning",
      startX: e.clientX - panX,
      startY: e.clientY - panY,
    };
    e.preventDefault();
  });

  // Hover cursor feedback (only when idle — during a drag the browser
  // holds the grab cursor automatically).
  svgViewport.addEventListener("mousemove", (e: MouseEvent) => {
    if (interaction.kind !== "idle") return;
    const rect = svgViewport.getBoundingClientRect();
    const sx = e.clientX - rect.left;
    const sy = e.clientY - rect.top;
    const mm = cursorToMm(e);
    let cursor = "";
    if (selectedWhimsyId !== null) {
      const sel = whimsies.find((w) => w.id === selectedWhimsyId);
      if (sel) {
        const hit = handleHitTest(sel, sx, sy);
        if (hit === "rotate") cursor = "grab";
        else if (hit !== null) cursor = cursorForCorner(hit);
      }
    }
    if (!cursor && findWhimsyAt(mm.x, mm.y)) cursor = "move";
    svgViewport.style.cursor = cursor;
  });

  // Drag dispatch on window so motion outside the viewport still counts.
  window.addEventListener("mousemove", (e: MouseEvent) => {
    if (interaction.kind === "idle") return;

    if (interaction.kind === "panning") {
      panX = e.clientX - interaction.startX;
      panY = e.clientY - interaction.startY;
      scheduleTransform();
      return;
    }

    const whimsyId = interaction.id;
    const w = whimsies.find((x) => x.id === whimsyId);
    if (!w) return;
    const mm = cursorToMm(e);

    // Each branch builds a candidate copy of `w` with the new
    // geometry, validates it against the clearance rules, and only
    // mutates `w` if the candidate is valid. A candidate that
    // collides with another whimsy or runs past the edge is silently
    // ignored — the cursor moves but the whimsy stops at its last
    // valid pose, snapping back into a legal state on release.
    // Whimsy manipulation: only the ghost overlay updates during the
    // drag (via `scheduleTransform`). The puzzle layout regen runs
    // once on mouseup, against the final whimsy position. Threshold
    // gating still applies so a click-without-drag doesn't regen.
    if (interaction.kind === "dragging-whimsy") {
      const candidate: WhimsyInstance = {
        ...w,
        centerX: mm.x + interaction.offsetX,
        centerY: mm.y + interaction.offsetY,
      };
      if (whimsyPlacementValid(candidate, whimsies)) {
        w.centerX = candidate.centerX;
        w.centerY = candidate.centerY;
        scheduleTransform();
        if (!interaction.committed) {
          const dx = w.centerX - interaction.startCenterX;
          const dy = w.centerY - interaction.startCenterY;
          if (
            dx * dx + dy * dy >=
            DRAG_REGEN_THRESHOLD_MM * DRAG_REGEN_THRESHOLD_MM
          ) {
            interaction.committed = true;
          }
        }
      }
    } else if (interaction.kind === "scaling-whimsy") {
      const loc = whimsyLocal(w, mm.x, mm.y);
      const dist = Math.max(Math.abs(loc.x), Math.abs(loc.y));
      const ratio = dist / interaction.initialDist;
      const candidate: WhimsyInstance = {
        ...w,
        size: Math.max(10, interaction.initialSize * ratio),
      };
      if (whimsyPlacementValid(candidate, whimsies)) {
        w.size = candidate.size;
        scheduleTransform();
        if (!interaction.committed) {
          const sizeRatio = w.size / interaction.initialSize;
          if (Math.abs(sizeRatio - 1) >= SCALE_REGEN_THRESHOLD_RATIO) {
            interaction.committed = true;
          }
        }
      }
    } else if (interaction.kind === "rotating-whimsy") {
      const angDeg =
        (Math.atan2(mm.y - w.centerY, mm.x - w.centerX) * 180) / Math.PI;
      let rot =
        interaction.initialRotation + (angDeg - interaction.initialAngleDeg);
      rot = ((rot % 360) + 360) % 360;
      const candidate: WhimsyInstance = { ...w, rotation: rot };
      if (whimsyPlacementValid(candidate, whimsies)) {
        w.rotation = rot;
        scheduleTransform();
        if (!interaction.committed) {
          let delta = Math.abs(w.rotation - interaction.initialRotation);
          // Wrap around so 359° vs 1° reads as a 2° delta, not 358°.
          if (delta > 180) delta = 360 - delta;
          if (delta >= ROTATE_REGEN_THRESHOLD_DEG) {
            interaction.committed = true;
          }
        }
      }
    }
  });

  window.addEventListener("mouseup", () => {
    // Capture committedness before flipping back to idle. If the
    // user never moved past the regen threshold, nothing about the
    // puzzle has changed and there's no need to commit a regen —
    // the canonical layout that's already on screen is still
    // canonical.
    const committed =
      (interaction.kind === "dragging-whimsy" ||
        interaction.kind === "scaling-whimsy" ||
        interaction.kind === "rotating-whimsy") &&
      interaction.committed;
    const wasWhimsy =
      interaction.kind === "dragging-whimsy" ||
      interaction.kind === "scaling-whimsy" ||
      interaction.kind === "rotating-whimsy";
    interaction = { kind: "idle" };
    if (wasWhimsy) {
      renderWhimsies();
      if (committed) scheduleGenerate();
    }
  });

  // Double-click to reset zoom
  svgViewport.addEventListener("dblclick", () => {
    resetZoom();
  });

  // Zoom button handlers
  zoomInBtn.addEventListener("click", () => {
    const rect = svgViewport.getBoundingClientRect();
    const cx = rect.width / 2;
    const cy = rect.height / 2;
    const oldZoom = zoomLevel;
    zoomLevel = Math.min(MAX_ZOOM, zoomLevel * ZOOM_STEP);
    const zoomRatio = zoomLevel / oldZoom;
    panX = cx - zoomRatio * (cx - panX);
    panY = cy - zoomRatio * (cy - panY);
    applyTransform();
  });

  zoomOutBtn.addEventListener("click", () => {
    const rect = svgViewport.getBoundingClientRect();
    const cx = rect.width / 2;
    const cy = rect.height / 2;
    const oldZoom = zoomLevel;
    zoomLevel = Math.max(MIN_ZOOM, zoomLevel / ZOOM_STEP);
    const zoomRatio = zoomLevel / oldZoom;
    panX = cx - zoomRatio * (cx - panX);
    panY = cy - zoomRatio * (cy - panY);
    applyTransform();
  });

  zoomResetBtn.addEventListener("click", () => {
    resetZoom();
  });

  // Touch support — pinch zoom and drag
  let lastTouchDist = 0;

  svgViewport.addEventListener(
    "touchstart",
    (e: TouchEvent) => {
      if (e.touches.length === 1) {
        interaction = {
          kind: "panning",
          startX: e.touches[0].clientX - panX,
          startY: e.touches[0].clientY - panY,
        };
      } else if (e.touches.length === 2) {
        interaction = { kind: "idle" };
        const dx = e.touches[0].clientX - e.touches[1].clientX;
        const dy = e.touches[0].clientY - e.touches[1].clientY;
        lastTouchDist = Math.sqrt(dx * dx + dy * dy);
      }
    },
    { passive: true },
  );

  svgViewport.addEventListener(
    "touchmove",
    (e: TouchEvent) => {
      e.preventDefault();
      if (e.touches.length === 1 && interaction.kind === "panning") {
        panX = e.touches[0].clientX - interaction.startX;
        panY = e.touches[0].clientY - interaction.startY;
        scheduleTransform();
      } else if (e.touches.length === 2) {
        const dx = e.touches[0].clientX - e.touches[1].clientX;
        const dy = e.touches[0].clientY - e.touches[1].clientY;
        const dist = Math.sqrt(dx * dx + dy * dy);
        const midX = (e.touches[0].clientX + e.touches[1].clientX) / 2;
        const midY = (e.touches[0].clientY + e.touches[1].clientY) / 2;
        const rect = svgViewport.getBoundingClientRect();

        if (lastTouchDist > 0) {
          const oldZoom = zoomLevel;
          zoomLevel = Math.max(
            MIN_ZOOM,
            Math.min(MAX_ZOOM, zoomLevel * (dist / lastTouchDist)),
          );
          const zoomRatio = zoomLevel / oldZoom;
          const cx = midX - rect.left;
          const cy = midY - rect.top;
          panX = cx - zoomRatio * (cx - panX);
          panY = cy - zoomRatio * (cy - panY);
          scheduleTransform();
        }

        lastTouchDist = dist;
      }
    },
    { passive: false },
  );

  svgViewport.addEventListener("touchend", () => {
    interaction = { kind: "idle" };
    lastTouchDist = 0;
  });

  // ─── Download SVG ──────────────────────────────────────

  const downloadBtn = document.getElementById("download")!;
  downloadBtn.addEventListener("click", async () => {
    const svgContent = await requestCachedSvg().catch(() => "");
    if (!svgContent || !svgContent.startsWith("<svg")) return;
    const config = buildConfig() as Record<string, unknown>;
    const pc = generatedPieceCount || (config.piece_count as number);
    const border = config.border_shape as string | undefined;
    const parts = ["puzzle", `${pc}pc`];
    if (isClassicGrid()) parts.push("classic");
    if (border && border !== "rectangle") parts.push(border);
    if (!isClassicGrid() && whimsies.length > 0) {
      parts.push(
        `${whimsies.length}whims${whimsies.length === 1 ? "y" : "ies"}`,
      );
    }
    const seed = (config.seed as string) || "seed";
    parts.push(seed);
    const filename = `${parts.join("-")}.svg`;
    const blob = new Blob([svgContent], { type: "image/svg+xml" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  });

  // ─── Copy Link ─────────────────────────────────────────

  const copyLinkBtn = document.getElementById("copy-link")!;
  copyLinkBtn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(window.location.href);
      const original = copyLinkBtn.textContent;
      copyLinkBtn.textContent = "Copied!";
      setTimeout(() => {
        copyLinkBtn.textContent = original;
      }, 1500);
    } catch {
      // Fallback for non-HTTPS contexts
      const input = document.createElement("input");
      input.value = window.location.href;
      document.body.appendChild(input);
      input.select();
      document.execCommand("copy");
      document.body.removeChild(input);
      const original = copyLinkBtn.textContent;
      copyLinkBtn.textContent = "Copied!";
      setTimeout(() => {
        copyLinkBtn.textContent = original;
      }, 1500);
    }
  });

  // ─── Initial Generate ─────────────────────────────────────

  enforceConstraints();
  generatePuzzle();
  resetZoom();
}

main();
