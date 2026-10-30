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

## CI and GitHub Pages

The [CI and Pages workflow](.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and manual dispatches. It installs Node.js 24 and stable Rust with the WebAssembly target, runs the Rust tests, builds the production WASM and Vite app, and checks TypeScript. npm, Rust dependencies, and build tools are cached. The resulting `web/dist` is uploaded as the `github-pages` artifact, retained for seven days.

After those checks pass on `main`, a deployment job publishes **that same CI-built artifact** to GitHub Pages. Pull requests build and upload an artifact but do not deploy. Deployment uses GitHub’s built-in token and the `github-pages` environment; no personal access token or generated `gh-pages` branch is needed.

Enable **Settings → Pages → Build and deployment → Source: GitHub Actions** in the repository before the first deployment. See [GitHub’s Pages setup documentation](https://docs.github.com/en/pages/getting-started-with-github-pages/configuring-a-publishing-source-for-your-github-pages-site). Push to `main` or run **Actions → CI and Pages → Run workflow** to publish. The deployment job reports the live URL (normally `https://<owner>.github.io/JigsawGenerator/`).

The Vite build uses relative asset URLs, so the static app, Web Worker, and WASM binary also load correctly under a repository subdirectory. To check locally after building, run `npm run typecheck` and `npm run preview` from `web/`.

## Current State

The generator offers three algorithms:

- **Classic grid** — traditional four-sided pieces with matching tabs and sockets. Rows and columns are chosen for roughly square pieces; the target count is rounded to a complete grid (for example, 48 pieces at 297 × 210 mm gives an 8 × 6 grid). The footer shows the actual count and grid dimensions. Classic currently uses rectangular borders without whimsies.
- **CVT** — relaxed Voronoi cells with organic, typically five- or six-sided pieces.
- **Poisson** — Voronoi cells from Poisson disc sampling, with optional relaxation. Both Voronoi options support shaped borders and whimsy pieces.

Choose an algorithm in **Pieces → Algorithm**. Existing shared links keep their original algorithm; classic links include `algo=classic`.

The **Knobs** section provides live sliders for **Knob size** (50–125%) and **Knob position randomness** (0–100%) in **all three algorithms**. Size and placement propagate through collision handling, shaped borders, and nested whimsy sub-puzzles. Tight spaces can still adjust individual knobs. Variations are deterministic for a given seed and saved in shared links; existing classic links continue to work.

**Classic grid style → Grid irregularity** (0–100%) moves corners and gently curves sides while retaining four-sided pieces. This control is specific to Classic; CVT and Poisson already generate irregular cells. Zero irregularity and randomness reproduce the original regular grid. Grid curvature remains visible when knobs are disabled.

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

## SVG export and Epilog line weights

The default SVG cut stroke is **0.001 inch = 0.0254 mm = 0.072 pt**, with a black stroke, no fill, and unchanged physical dimensions and centerline geometry. It is exported as `stroke-width="0.0254"`: the document’s viewBox maps one unit to one millimeter. A `mm` or `pt` suffix on the stroke would apply an extra CSS-unit conversion. The previous `stroke-width="0.001mm"` rendered at approximately 0.00378 mm under SVG’s 96-dpi unit rules, substantially thinner than the new cut weight.

Line-weight recognition is a manufacturer/software/driver convention, not a universal laser-cutting standard. The [Fusion M2 32/40 manual, “Setting a Vector Cutting Line”, p. 84](https://www.epiloglaser.com/assets/downloads/manuals/fusion-manual-web.pdf) lists widths up to 0.004 inch as vector cuts across its listed resolutions. [Epilog’s current Illustrator guidance](https://support.epiloglaser.com/laser-machine/legend-36ext/usage-and-operation/system-requirements-setup/sending-vector-jobs-from-adobe-illustrator/) specifies a more conservative maximum of 0.001 inch (0.072 pt). **0.1 pt is about 0.0353 mm**, so it fits the older manual’s table but exceeds that current Illustrator threshold. Verify vector recognition in the driver preview for your actual software and driver.

**SVG export → Stroke width** offers the default Epilog cut profile, a driver-dependent 0.1 pt option, and a **0.25 mm editing copy** with an `-editing.svg` filename. Editing copies need their stroke reset to a cut weight before laser output. Export choices are included in shared links and do not regenerate the puzzle.

For visibility in Inkscape, use **View → Display Mode → Visible Hairlines**. This changes the on-screen display without changing the saved stroke width; see [Inkscape’s documentation](https://wiki.inkscape.org/wiki/Release_notes/1.0#Visible_Hairlines_Display_Mode). Stroke width does not compensate for laser kerf.

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
