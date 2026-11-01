use std::ops::Range;

/// A run of UTF-16 code units that shaped into one cluster of glyphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphCluster {
    /// Number of UTF-16 code units in the cluster. Clusters are returned in logical (text) order,
    /// so the offset of a cluster is the sum of the lengths before it.
    pub utf16_len: usize,
    /// The cluster's glyphs, as indices into the run's glyph arrays.
    pub glyphs: Range<usize>,
}

/// Pairs the clusters of a shaped run with its glyphs from a DirectWrite-style cluster map, which
/// holds for each UTF-16 code unit the index of the first glyph of its cluster.
///
/// DirectWrite hands the glyphs and the cluster map of a run in logical order, whatever the run's
/// bidi level, so the map is non-decreasing for left-to-right and right-to-left runs alike. Only
/// the placement differs; see [`glyph_run_positions`].
pub fn glyph_clusters(cluster_map: &[u16], glyph_count: usize) -> Vec<GlyphCluster> {
    let mut clusters: Vec<GlyphCluster> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut utf16_ix = 0;
    while utf16_ix < cluster_map.len() {
        let first_glyph = cluster_map[utf16_ix];
        let utf16_len = cluster_map[utf16_ix..]
            .iter()
            .take_while(|&&glyph| glyph == first_glyph)
            .count();
        let start = (first_glyph as usize).min(glyph_count);
        starts.push(start);
        clusters.push(GlyphCluster {
            utf16_len,
            glyphs: start..start,
        });
        utf16_ix += utf16_len;
    }
    for (cluster_ix, cluster) in clusters.iter_mut().enumerate() {
        let next_start = starts.get(cluster_ix + 1).copied().unwrap_or(glyph_count);
        cluster.glyphs.end = next_start.max(cluster.glyphs.start);
    }
    clusters
}

/// A glyph of a shaped run and where its left edge is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    /// The glyph's index in the run's glyph arrays, which are in logical order.
    pub glyph: usize,
    /// The left edge of the glyph, in the line's coordinates.
    pub x: f32,
}

/// Where each glyph of a run is drawn, and the run's total advance. The glyphs come back in the
/// order a [`LineLayout`](super::LineLayout) expects, left to right, whatever the run's direction.
///
/// A left-to-right run is laid out from `run_left` toward the right. DirectWrite expects the
/// renderer to lay a right-to-left run out from its right edge toward the left, so the first
/// logical glyph sits at the right and each glyph's left edge is the run's right edge minus the
/// advances up to and including that glyph. An advance offset moves a glyph along the direction
/// of advance, which is leftward in a right-to-left run. A right-to-left run is returned from its
/// last logical glyph to its first; each glyph keeps its [`PlacedGlyph::glyph`], so its cluster
/// keeps its text index.
///
/// This is the shape CoreText gives GPUI on macOS: glyphs left to right, with string indices
/// descending inside a right-to-left run. Only the glyph order is bidi-aware, not the consumers
/// of a layout: `index_for_x` finds the glyph drawn at a point, while `x_for_index` and
/// `closest_index_for_x` still read indices as ascending, so a caret in right-to-left text lands
/// where it does on macOS.
pub fn glyph_run_positions(
    advances: &[f32],
    advance_offsets: &[f32],
    run_left: f32,
    right_to_left: bool,
) -> (Vec<PlacedGlyph>, f32) {
    debug_assert_eq!(
        advances.len(),
        advance_offsets.len(),
        "a glyph without an offset would be dropped by the zip"
    );
    let total: f32 = advances.iter().sum();
    let mut pen = 0.0;
    let mut placed: Vec<PlacedGlyph> = advances
        .iter()
        .zip(advance_offsets)
        .enumerate()
        .map(|(glyph, (advance, offset))| {
            let x = if right_to_left {
                pen += advance;
                run_left + total - pen - offset
            } else {
                let x = run_left + pen + offset;
                pen += advance;
                x
            };
            PlacedGlyph { glyph, x }
        })
        .collect();
    if right_to_left {
        placed.reverse();
    }
    (placed, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontId, GlyphId, LineLayout, ShapedGlyph, ShapedRun, point, px};

    fn cluster(utf16_len: usize, glyphs: Range<usize>) -> GlyphCluster {
        GlyphCluster { utf16_len, glyphs }
    }

    #[test]
    fn one_glyph_per_character() {
        assert_eq!(glyph_clusters(&[0], 1), vec![cluster(1, 0..1)]);
        assert_eq!(
            glyph_clusters(&[0, 1, 2], 3),
            vec![cluster(1, 0..1), cluster(1, 1..2), cluster(1, 2..3)]
        );
    }

    #[test]
    fn a_cluster_may_span_several_characters_and_glyphs() {
        // A family emoji (11 code units, 4 glyphs) followed by a technologist emoji (5, 1).
        let map = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 4];
        assert_eq!(
            glyph_clusters(&map, 5),
            vec![cluster(11, 0..4), cluster(5, 4..5)]
        );
        assert_eq!(glyph_clusters(&[0, 0, 0, 0, 0], 1), vec![cluster(5, 0..1)]);
    }

    #[test]
    fn a_ligature_covers_several_characters_with_one_glyph() {
        // Lam-alef: two code units shape to one glyph, the third has its own.
        assert_eq!(
            glyph_clusters(&[0, 0, 1], 2),
            vec![cluster(2, 0..1), cluster(1, 1..2)]
        );
    }

    #[test]
    fn a_character_may_shape_to_several_glyphs() {
        assert_eq!(
            glyph_clusters(&[0, 2, 3], 4),
            vec![cluster(1, 0..2), cluster(1, 2..3), cluster(1, 3..4)]
        );
    }

    #[test]
    fn every_glyph_belongs_to_exactly_one_cluster() {
        let mut owner = vec![0; 5];
        for cluster in glyph_clusters(&[0, 0, 1, 3, 3], 5) {
            for glyph in cluster.glyphs {
                owner[glyph] += 1;
            }
        }
        assert_eq!(owner, vec![1; 5]);
    }

    #[test]
    fn damaged_maps_never_produce_an_inverted_or_out_of_range_cluster() {
        for map in [&[0u16, 9][..], &[3, 1, 0][..]] {
            for cluster in glyph_clusters(map, 2) {
                assert!(cluster.glyphs.start <= cluster.glyphs.end);
                assert!(cluster.glyphs.end <= 2);
            }
        }
    }

    fn placed(positions: &[PlacedGlyph]) -> Vec<(usize, f32)> {
        positions.iter().map(|p| (p.glyph, p.x)).collect()
    }

    #[test]
    fn left_to_right_glyphs_advance_rightward_from_the_run_start() {
        let (positions, total) =
            glyph_run_positions(&[10.0, 20.0, 5.0], &[0.0, 1.0, 0.0], 100.0, false);
        assert_eq!(placed(&positions), vec![(0, 100.0), (1, 111.0), (2, 130.0)]);
        assert_eq!(total, 35.0);
    }

    #[test]
    fn right_to_left_first_logical_glyph_sits_at_the_right_edge() {
        let (positions, total) =
            glyph_run_positions(&[10.0, 20.0, 5.0], &[0.0, 0.0, 0.0], 100.0, true);
        // Right edge is 135; glyph 0 occupies 125..135, glyph 1 105..125, glyph 2 100..105.
        // They are returned left to right, so glyph 2 comes first.
        assert_eq!(placed(&positions), vec![(2, 100.0), (1, 105.0), (0, 125.0)]);
        assert_eq!(total, 35.0);
    }

    #[test]
    fn right_to_left_advance_offset_moves_the_glyph_leftward() {
        let (positions, _) = glyph_run_positions(&[10.0], &[3.0], 0.0, true);
        assert_eq!(placed(&positions), vec![(0, -3.0)]);
    }

    /// One shaped run the way the Windows text system assembles it: every glyph keeps the UTF-8
    /// offset of its cluster in the line (`run_start` is where the run begins), and is placed where `glyph_run_positions` says.
    fn shaped_run(
        run_start: usize,
        run_text: &str,
        cluster_map: &[u16],
        advances: &[f32],
        run_left: f32,
        right_to_left: bool,
    ) -> (ShapedRun, f32) {
        let mut text_ix = vec![0; advances.len()];
        let unit_starts: Vec<usize> = run_text
            .char_indices()
            .flat_map(|(ix, ch)| std::iter::repeat_n(run_start + ix, ch.len_utf16()))
            .collect();
        let mut utf16_ix = 0;
        for cluster in glyph_clusters(cluster_map, advances.len()) {
            text_ix[cluster.glyphs].fill(unit_starts[utf16_ix]);
            utf16_ix += cluster.utf16_len;
        }
        let offsets = vec![0.0; advances.len()];
        let (placed, width) = glyph_run_positions(advances, &offsets, run_left, right_to_left);
        let glyphs = placed
            .into_iter()
            .map(|PlacedGlyph { glyph, x }| ShapedGlyph {
                id: GlyphId(glyph as u32),
                position: point(px(x), px(0.)),
                index: text_ix[glyph],
                is_emoji: false,
            })
            .collect();
        (
            ShapedRun {
                font_id: FontId(0),
                glyphs,
            },
            width,
        )
    }

    fn line_of(text: &str, runs: Vec<ShapedRun>, width: f32) -> LineLayout {
        LineLayout {
            width: px(width),
            runs,
            len: text.len(),
            ..Default::default()
        }
    }

    fn emitted_x(run: &ShapedRun) -> Vec<f32> {
        run.glyphs.iter().map(|glyph| glyph.position.x.0).collect()
    }

    #[test]
    fn a_right_to_left_run_is_emitted_left_to_right() {
        let text = "\u{5d0}\u{5d1}\u{5d2}";
        let (run, width) = shaped_run(0, text, &[0, 1, 2], &[10.0, 20.0, 30.0], 0.0, true);
        assert_eq!(emitted_x(&run), vec![0.0, 30.0, 50.0]);
        assert_eq!(width, 60.0);
        // Each glyph keeps its own text index: the first letter is drawn at the right.
        let drawn_indices: Vec<usize> = run.glyphs.iter().map(|glyph| glyph.index).collect();
        assert_eq!(drawn_indices, vec![4, 2, 0]);
    }

    #[test]
    fn hit_testing_a_right_to_left_run_finds_the_glyph_drawn_at_x() {
        let text = "\u{5d0}\u{5d1}\u{5d2}";
        let (run, width) = shaped_run(0, text, &[0, 1, 2], &[10.0, 20.0, 30.0], 0.0, true);
        let line = line_of(text, vec![run], width);
        // Letter 0 is drawn over 50..60, letter 1 over 30..50, letter 2 over 0..30.
        for (x, index) in [
            (55.0, 0),
            (50.0, 0),
            (40.0, 2),
            (30.0, 2),
            (10.0, 4),
            (0.0, 4),
        ] {
            assert_eq!(line.index_for_x(px(x)), Some(index), "x = {x}");
        }
        assert_eq!(line.index_for_x(px(60.0)), None);
    }

    #[test]
    fn hit_testing_a_right_to_left_ligature_finds_the_ligature_and_the_letter_after_it() {
        // Lam-alef shapes to one glyph, drawn at the right, then beh to its left.
        let text = "\u{644}\u{627}\u{628}";
        let (run, width) = shaped_run(0, text, &[0, 0, 1], &[12.0, 8.0], 0.0, true);
        let line = line_of(text, vec![run], width);
        assert_eq!(line.index_for_x(px(15.0)), Some(0));
        assert_eq!(line.index_for_x(px(3.0)), Some(4));
    }

    #[test]
    fn a_left_to_right_run_is_unchanged() {
        let text = "abc";
        let (run, width) = shaped_run(0, text, &[0, 1, 2], &[10.0, 20.0, 30.0], 5.0, false);
        assert_eq!(emitted_x(&run), vec![5.0, 15.0, 35.0]);
        assert_eq!(width, 60.0);
        let line = line_of(text, vec![run], width + 5.0);
        for (x, index) in [
            (5.0, 0),
            (14.0, 0),
            (15.0, 1),
            (34.0, 1),
            (35.0, 2),
            (64.0, 2),
        ] {
            assert_eq!(line.index_for_x(px(x)), Some(index), "x = {x}");
        }
    }

    #[test]
    fn a_line_of_mixed_directions_ascends_across_its_runs() {
        // "ab", two Hebrew letters, "cd": the runs are joined at the width of those before them.
        let text = "ab\u{5d0}\u{5d1}cd";
        let pieces: [(usize, &str, bool); 3] = [
            (0, "ab", false),
            (2, "\u{5d0}\u{5d1}", true),
            (6, "cd", false),
        ];
        let mut runs = Vec::new();
        let mut width = 0.0;
        for (start, run_text, right_to_left) in pieces {
            let map: Vec<u16> = (0..run_text.encode_utf16().count() as u16).collect();
            let advances = vec![10.0; map.len()];
            let (run, run_width) =
                shaped_run(start, run_text, &map, &advances, width, right_to_left);
            width += run_width;
            runs.push(run);
        }
        let line = line_of(text, runs, width);
        let xs: Vec<f32> = line.runs.iter().flat_map(emitted_x).collect();
        assert!(xs.windows(2).all(|pair| pair[0] < pair[1]), "{xs:?}");
        // a, b, then the second Hebrew letter (drawn first), the first one, then c, d.
        for (x, index) in [
            (5.0, 0),
            (15.0, 1),
            (25.0, 4),
            (35.0, 2),
            (45.0, 6),
            (55.0, 7),
        ] {
            assert_eq!(line.index_for_x(px(x)), Some(index), "x = {x}");
        }
    }

    #[test]
    fn right_to_left_ligature_cluster_places_its_glyph_once() {
        // Two characters, one ligature glyph, then a plain glyph: the ligature is the rightmost.
        let clusters = glyph_clusters(&[0, 0, 1], 2);
        let (positions, _) = glyph_run_positions(&[12.0, 8.0], &[0.0, 0.0], 0.0, true);
        assert_eq!(clusters[0].glyphs, 0..1);
        assert_eq!(placed(&positions), vec![(1, 0.0), (0, 8.0)]);
    }
}
