/**
 * Main-thread client for `regen-worker.ts`.
 *
 * Wraps the postMessage protocol in Promise-returning calls. Stateful
 * request management:
 *
 * - `requestBuild` is **coalescing**: if a build is already in flight
 *   when a new one arrives, the in-flight one's result is discarded
 *   on arrival and the latest queued one is sent. Only the *most
 *   recent* build's result reaches the caller. Implementation: when
 *   a new build comes in, the previous unresolved promise is
 *   immediately rejected with `BUILD_SUPERSEDED` so callers can
 *   distinguish "ignore this result" from real failures.
 *
 * - `requestShapeUnitPath` and `requestCachedSvg` are sequential —
 *   they queue normally and resolve in order. They're rare (one per
 *   whimsy creation, one per SVG download) so coalescing isn't worth
 *   the added complexity.
 */

// Vite picks this up via the `?worker` query param and bundles the
// worker file as a separate chunk. ESM module workers — needed
// because puzzle-wasm uses ES module imports.
import RegenWorker from "./regen-worker?worker";

export type BuildResult = {
  edges?: Float64Array;
  border?: Float64Array;
  centers?: Float64Array;
  anchors?: Float64Array;
  width: number;
  height: number;
  piece_count?: number;
  grid_columns?: number;
  grid_rows?: number;
};

/// Sentinel rejection value: a newer `requestBuild` invalidated this
/// one before the worker responded. Callers should handle this
/// silently rather than treating it as an error.
export const BUILD_SUPERSEDED = Symbol("build superseded");

type PendingResolver<T> = {
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
};

const worker = new RegenWorker();
let nextId = 1;
const pending = new Map<number, PendingResolver<unknown>>();
let inflightBuildId: number | null = null;

worker.onmessage = (e: MessageEvent) => {
  const { id, error, kind } = e.data as {
    id: number;
    error?: string;
    kind?: string;
  };
  const resolver = pending.get(id);
  if (!resolver) {
    // Resolver was already rejected (e.g., superseded) — drop
    // the stale response.
    return;
  }
  pending.delete(id);
  if (kind === "build" && id === inflightBuildId) {
    inflightBuildId = null;
  }
  if (error !== undefined) {
    resolver.reject(new Error(error));
  } else {
    resolver.resolve(e.data);
  }
};

worker.onerror = (e: ErrorEvent) => {
  console.error("[regen-worker] error:", e.message);
};

export function requestBuild(configJson: string): Promise<BuildResult> {
  // If a build is in flight, supersede it: reject its promise and
  // forget about its eventual response. We still leave its entry in
  // `pending` (with a no-op resolver) so the worker's onmessage doesn't
  // throw.
  if (inflightBuildId !== null) {
    const prev = pending.get(inflightBuildId);
    if (prev) {
      prev.reject(BUILD_SUPERSEDED);
      pending.set(inflightBuildId, {
        resolve: () => {},
        reject: () => {},
      });
    }
  }

  const id = nextId++;
  inflightBuildId = id;
  return new Promise<BuildResult>((resolve, reject) => {
    pending.set(id, {
      resolve: (data: unknown) => {
        const d = data as { result: BuildResult };
        resolve(d.result);
      },
      reject,
    });
    worker.postMessage({ id, kind: "build", config_json: configJson });
  });
}

export function requestShapeUnitPath(shape: string): Promise<Float64Array> {
  const id = nextId++;
  return new Promise<Float64Array>((resolve, reject) => {
    pending.set(id, {
      resolve: (data: unknown) => {
        const d = data as { path: Float64Array };
        resolve(d.path);
      },
      reject,
    });
    worker.postMessage({ id, kind: "shape_unit_path", shape });
  });
}

export function requestCachedSvg(): Promise<string> {
  const id = nextId++;
  return new Promise<string>((resolve, reject) => {
    pending.set(id, {
      resolve: (data: unknown) => {
        const d = data as { svg: string };
        resolve(d.svg);
      },
      reject,
    });
    worker.postMessage({ id, kind: "cached_svg" });
  });
}
