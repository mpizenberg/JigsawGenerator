/**
 * WASM regeneration worker.
 *
 * Owns the only `puzzle_wasm` instance in the app. Main thread talks to
 * this worker via the message protocol below; never imports the WASM
 * module directly. Result: every CVT regenerate runs off-main, the UI
 * thread stays at 60fps regardless of how long generation takes.
 *
 * Requests are processed serially in arrival order. Coalescing /
 * staleness handling lives on the main side — see `worker-client.ts`.
 *
 * # Protocol
 *
 * Each message has an `id` (u32) so responses can be matched to
 * requests. Three request types:
 *
 *   { id, kind: "build", config_json }
 *     → { id, kind: "build", result: { edges, border, centers,
 *                                       anchors, width, height,
 *                                       piece_count } }
 *     The TypedArray buffers in the result are transferred (zero-copy).
 *
 *   { id, kind: "shape_unit_path", shape }
 *     → { id, kind: "shape_unit_path", path: Float64Array }
 *
 *   { id, kind: "cached_svg" }
 *     → { id, kind: "cached_svg", svg: string }
 *
 * Errors come back as `{ id, error: string }`.
 */

import init, {
  generate_edges_binary,
  get_cached_svg,
  get_shape_unit_path,
  init_panic_hook,
} from "puzzle-wasm";

type BuildRequest = {
  id: number;
  kind: "build";
  config_json: string;
};

type ShapeUnitPathRequest = {
  id: number;
  kind: "shape_unit_path";
  shape: string;
};

type CachedSvgRequest = {
  id: number;
  kind: "cached_svg";
};

type Request = BuildRequest | ShapeUnitPathRequest | CachedSvgRequest;

let wasmReady: Promise<void> | null = null;

function ensureWasm(): Promise<void> {
  if (wasmReady === null) {
    wasmReady = (async () => {
      await init();
      init_panic_hook();
    })();
  }
  return wasmReady;
}

/**
 * Build response shape — what the WASM `generate_edges_binary` returns,
 * with TypedArray fields the worker will transfer back to main.
 */
type BuildResultRaw = {
  error?: string;
  edges?: Float64Array;
  border?: Float64Array;
  centers?: Float64Array;
  anchors?: Float64Array;
  width?: number;
  height?: number;
  piece_count?: number;
  grid_columns?: number;
  grid_rows?: number;
};

self.onmessage = async (e: MessageEvent<Request>) => {
  const req = e.data;
  try {
    await ensureWasm();
  } catch (err) {
    (self as unknown as Worker).postMessage({
      id: req.id,
      error: `WASM init failed: ${err}`,
    });
    return;
  }

  if (req.kind === "build") {
    let result: BuildResultRaw;
    try {
      result = generate_edges_binary(req.config_json) as BuildResultRaw;
    } catch (err) {
      (self as unknown as Worker).postMessage({
        id: req.id,
        error: `build failed: ${err}`,
      });
      return;
    }
    if (result && result.error) {
      (self as unknown as Worker).postMessage({
        id: req.id,
        error: result.error,
      });
      return;
    }
    // Collect TypedArray buffers to transfer (zero-copy). Skip
    // any that are absent (centers/anchors are optional, the
    // others should always be present on success). Cast to
    // ArrayBuffer for postMessage's transfer-list signature
    // (TypedArray.buffer is ArrayBufferLike, which is wider).
    const transfers: ArrayBuffer[] = [];
    for (const buf of [
      result.edges?.buffer,
      result.border?.buffer,
      result.centers?.buffer,
      result.anchors?.buffer,
    ]) {
      if (buf) transfers.push(buf as ArrayBuffer);
    }
    (self as unknown as Worker).postMessage(
      {
        id: req.id,
        kind: "build",
        result: {
          edges: result.edges,
          border: result.border,
          centers: result.centers,
          anchors: result.anchors,
          width: result.width,
          height: result.height,
          piece_count: result.piece_count,
          grid_columns: result.grid_columns,
          grid_rows: result.grid_rows,
        },
      },
      transfers,
    );
  } else if (req.kind === "shape_unit_path") {
    let path: Float64Array;
    try {
      path = get_shape_unit_path(req.shape);
    } catch (err) {
      (self as unknown as Worker).postMessage({
        id: req.id,
        error: `shape_unit_path failed: ${err}`,
      });
      return;
    }
    (self as unknown as Worker).postMessage(
      { id: req.id, kind: "shape_unit_path", path },
      [path.buffer as ArrayBuffer],
    );
  } else if (req.kind === "cached_svg") {
    let svg: string;
    try {
      svg = get_cached_svg();
    } catch (err) {
      (self as unknown as Worker).postMessage({
        id: req.id,
        error: `cached_svg failed: ${err}`,
      });
      return;
    }
    (self as unknown as Worker).postMessage({
      id: req.id,
      kind: "cached_svg",
      svg,
    });
  }
};
