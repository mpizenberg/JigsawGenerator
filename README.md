# Jigsaw Generator

A procedural jigsaw puzzle pattern generator that outputs SVG cut paths for laser cutting. Built as a Rust core compiled to WebAssembly with a vanilla TypeScript web GUI providing live preview.

Configure puzzle dimensions, piece counts, connector styles, and randomization in the browser, then export production-ready SVG files for cutting wood, acrylic, or other materials.

## Tech Stack

- **Core**: Rust (`crates/puzzle-core`) — grid model, connector generation, geometry, SVG/binary export
- **Bridge**: `wasm-bindgen` + `wasm-pack` (`crates/puzzle-wasm`) — JSON config in, binary edge data + cached SVG out
- **Frontend**: Vanilla TypeScript + Vite + Canvas 2D (`web/`) — live preview, controls, URL-based config sharing
- **Geometry**: [`kurbo`](https://crates.io/crates/kurbo) for points, bezier curves, affine transforms, BezPaths
- **RNG**: `rand_chacha` (ChaCha8) seeded via FNV-1a hash of a user-supplied seed string

## Getting Started

Prerequisites: Rust toolchain with `wasm32-unknown-unknown` target, `wasm-pack`, Node.js.

```bash
# From web/
npm install
npm run dev:wasm    # build WASM (debug)
npm run dev         # start Vite dev server
```

Production build:

```bash
npm run build       # builds WASM in release mode + Vite production bundle
```

After changing Rust code, re-run `npm run dev:wasm` — Vite does not trigger WASM rebuilds automatically.

## Current State

The generator offers three algorithms:

- **Classic grid** — traditional four-sided pieces with matching tabs and sockets. Rows and columns are chosen for roughly square pieces; the target count is rounded to a complete grid (for example, 48 pieces at 297 × 210 mm gives an 8 × 6 grid). The footer shows the actual count and grid dimensions. Classic currently uses rectangular borders without whimsies.
- **CVT** — relaxed Voronoi cells with organic, typically five- or six-sided pieces.
- **Poisson** — Voronoi cells from Poisson disc sampling, with optional relaxation. Both Voronoi options support shaped borders and whimsy pieces.

Choose an algorithm in **Pieces → Algorithm**. Existing shared links keep their original algorithm; classic links include `algo=classic`.

For Classic grid, **Classic grid style** provides live sliders for **Knob size** (50–125%), **Grid irregularity** (0–100%, moving corners and gently curving sides), and **Knob position randomness** (0–100%, centered to varied positions). Variations are deterministic for a given seed and saved in shared links. Zero irregularity and randomness reproduce the original regular grid. Knobs adapt locally where needed to keep neighboring cuts apart; the pieces retain four sides. Grid curvature remains visible when knobs are disabled.

Features include:

- Configurable grid (rows × columns) and physical dimensions in mm or inches
- Classic knob connectors with controls for tab size, taper, jitter, and offset randomization
- Per-parameter randomization toggles and lockable ranges
- Deterministic seed-based generation (same seed → identical puzzle)
- Canvas 2D live preview with zoom (0.5×–20×), pan, viewport culling, and touch support
- Smart constraint enforcement (piece size warnings, aspect ratio limits)
- SVG export with laser-cutter compatible stroke output
- URL-based configuration sharing via compact query parameters
- Piece count breakdown (total, edge, corner, interior)

## Planned Features

- **Custom borders** — non-rectangular puzzle outlines (heart, star, and an extensible shape library) via a mask operation on kurbo BezPaths
- **Whimsy pieces** — drag-and-drop placement of figural shapes onto the grid, with the whimsy contour itself acting as the cut line (no tabs at the boundary)
- **Whimsy resize** — interactive scaling with real-time grid adaptation
- **Sub-puzzle splitting** — subdivide the whimsy interior into multiple connected sub-pieces
- **Adaptive grid behavior** — piece count adjusts to fill non-rectangular shapes naturally rather than clipping a fixed grid
- **Multiple whimsies per puzzle** — including whimsy-whimsy intersection handling (deferred past v1)
- **User-imported SVG outlines** — upload arbitrary shapes for borders or whimsies
- **Multi-piece whimsies** — single whimsy shape spanning multiple grid cells
- **Arbitrary tessellations** — alternative piece tilings beyond the rectangular grid (hexagons, triangles, Penrose, Truchet, custom periodic and aperiodic tilings)

## Repository Layout

```
crates/
  puzzle-core/      Rust core: grid, edges, connectors, exporters
  puzzle-wasm/      wasm-bindgen bridge — JSON in, binary + SVG out
web/
  src/main.ts       Single-file vanilla-TS frontend
  src/style.css
  vite.config.ts
Cargo.toml          Workspace manifest
CLAUDE.md           Guidance for Claude Code agents
```

## Architecture Notes

- **Shared-edge model**: pieces reference edges by index into `h_edges` / `v_edges` arrays; each edge is stored once and shared between adjacent pieces.
- **Deterministic RNG**: grid construction and connector generation use separate ChaCha8 streams (seed suffix `-connectors`) so changes to one do not perturb the other.
- **Binary edge pipeline**: each edge is encoded as 36 floats (start/end + 5 cubic bezier control point pairs) and shipped to the frontend as a `Float64Array` for low-overhead Canvas rendering. The SVG export is a parallel pipeline cached inside the WASM module and retrieved on download.

## License

TBD.
