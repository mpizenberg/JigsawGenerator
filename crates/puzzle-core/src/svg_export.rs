//! Shared SVG primitives: the edge-local → global affine, and the SVG
//! document wrapper. The actual layout-to-SVG emission lives in
//! [`crate::layout::layout_generate_svg`], which both the rectangular
//! and future CVT builders feed into.

use kurbo::{Affine, Point};

/// Epilog's conservative vector-cut line weight: 0.001 inch = 0.072 pt.
/// Export this as a unitless length: the document viewBox maps 1 unit to 1 mm.
/// See https://support.epiloglaser.com/laser-machine/legend-36ext/usage-and-operation/system-requirements-setup/sending-vector-jobs-from-adobe-illustrator/
pub const DEFAULT_SVG_STROKE_MM: f64 = 0.0254;

/// Compute the affine transform from edge-local to global coordinates.
///
/// Edge-local: origin at edge start, x-axis along edge direction.
/// Global: the puzzle coordinate system in mm.
pub fn edge_transform(start: Point, end: Point) -> Affine {
    let angle = (end.y - start.y).atan2(end.x - start.x);
    Affine::translate(start.to_vec2()) * Affine::rotate(angle)
}

/// Wrap SVG path data in a complete SVG document.
///
/// Output format:
/// - Physical dimensions in mm (`width`, `height`)
/// - Matching `viewBox` for 1:1 mm coordinate mapping
/// - Single `<path>` element with hairline black stroke
/// - No fill, no metadata, no title/desc
pub(crate) fn build_svg_document(path_data: &str, width_mm: f64, height_mm: f64) -> String {
    format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{w}mm' height='{h}mm' viewBox='0 0 {w} {h}'>\
         <path d='{d}' stroke='#000000' stroke-width='{stroke}' fill='none'/>\
         </svg>",
        w = width_mm,
        h = height_mm,
        d = path_data,
        stroke = DEFAULT_SVG_STROKE_MM,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_transform_horizontal() {
        let t = edge_transform(Point::new(50.0, 0.0), Point::new(100.0, 0.0));
        // For a horizontal edge, angle = 0, transform is just translation.
        let p = t * Point::new(0.0, 0.0);
        assert!((p.x - 50.0).abs() < 1e-6 && (p.y - 0.0).abs() < 1e-6);

        let p = t * Point::new(50.0, 0.0);
        assert!((p.x - 100.0).abs() < 1e-6 && (p.y - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_edge_transform_vertical() {
        let t = edge_transform(Point::new(0.0, 50.0), Point::new(0.0, 100.0));
        // For a vertical downward edge, angle = PI/2.
        let p = t * Point::new(0.0, 0.0);
        assert!((p.x - 0.0).abs() < 1e-6 && (p.y - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_build_svg_document_structure() {
        let svg = build_svg_document("M 0 0 L 10 10", 50.0, 30.0);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("width='50mm'"));
        assert!(svg.contains("height='30mm'"));
        assert!(svg.contains("viewBox='0 0 50 30'"));
        assert!(svg.contains("xmlns='http://www.w3.org/2000/svg'"));
        assert!(svg.contains("stroke='#000000'"));
        assert!(svg.contains("stroke-width='0.0254'"));
        assert!((DEFAULT_SVG_STROKE_MM / 25.4 - 0.001).abs() < 1e-12);
        assert_eq!(svg.matches("<path ").count(), 1);
        assert!(svg.contains("fill='none'"));
        assert!(svg.contains("d='M 0 0 L 10 10'"));
    }
}
