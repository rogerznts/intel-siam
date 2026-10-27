use eframe::egui::{Rect, Vec2, pos2, vec2};

const ASPECT: f32 = 16.0 / 9.0;

/// Width of the largest 16:9 tile that fits `cols x rows` cells in `area`.
fn tile_width(cols: usize, rows: usize, area: Vec2) -> f32 {
    (area.x / cols as f32).min(area.y / rows as f32 * ASPECT).max(0.0)
}

/// `(cols, rows)` for `n` tiles that maximizes 16:9 tile area. Ties go to fewer empty cells, then to
/// more columns in a wide area and more rows in a tall one.
pub fn grid_dims(n: usize, area: Vec2) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let wide = area.x >= area.y;
    let mut best = (1, n);
    let mut best_w = tile_width(1, n, area);
    for cols in 2..=n {
        let rows = n.div_ceil(cols);
        let w = tile_width(cols, rows, area);
        let empty = cols * rows - n;
        let best_empty = best.0 * best.1 - n;
        let better = if (w - best_w).abs() > 0.5 {
            w > best_w
        } else if empty != best_empty {
            empty < best_empty
        } else {
            wide
        };
        if better {
            best = (cols, rows);
            best_w = w;
        }
    }
    best
}

/// Tile rectangles in grid order, row by row, with the grid centered in `area`.
pub fn tile_rects(area: Rect, n: usize) -> Vec<Rect> {
    let (cols, rows) = grid_dims(n, area.size());
    if n == 0 {
        return Vec::new();
    }
    let w = tile_width(cols, rows, area.size());
    let size = vec2(w, w / ASPECT);
    let grid = vec2(size.x * cols as f32, size.y * rows as f32);
    let origin = area.center() - grid / 2.0;
    (0..n)
        .map(|i| {
            let (col, row) = (i % cols, i / cols);
            Rect::from_min_size(pos2(origin.x + size.x * col as f32, origin.y + size.y * row as f32), size)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: Vec2 = vec2(1600.0, 900.0);
    const TALL: Vec2 = vec2(800.0, 1000.0);

    #[test]
    fn wide_window_dims_for_one_to_twelve() {
        let got: Vec<_> = (1..=12).map(|n| grid_dims(n, WIDE)).collect();
        let want = [
            (1, 1), (2, 1), (2, 2), (2, 2), (3, 2), (3, 2),
            (3, 3), (3, 3), (3, 3), (4, 3), (4, 3), (4, 3),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn six_cameras_are_three_by_two_wide_and_two_by_three_tall() {
        assert_eq!(grid_dims(6, WIDE), (3, 2));
        assert_eq!(grid_dims(6, TALL), (2, 3));
    }

    #[test]
    fn tall_window_stacks_few_tiles() {
        assert_eq!(grid_dims(2, TALL), (1, 2));
        assert_eq!(grid_dims(5, TALL), (2, 3));
    }

    #[test]
    fn no_row_or_column_is_ever_fully_empty() {
        for area in [WIDE, TALL, vec2(400.0, 1600.0), vec2(2400.0, 300.0)] {
            for n in 1..=12 {
                let (c, r) = grid_dims(n, area);
                assert!(c * r >= n, "{n} fits in {c}x{r}");
                assert!((c - 1) * r < n && c * (r - 1) < n, "{n} in {c}x{r} for {area:?}");
            }
        }
    }

    #[test]
    fn tiles_are_16_9_inside_area_and_do_not_overlap() {
        let area = Rect::from_min_size(pos2(10.0, 20.0), WIDE);
        let rects = tile_rects(area, 5);
        assert_eq!(rects.len(), 5);
        for (i, r) in rects.iter().enumerate() {
            assert!((r.width() / r.height() - ASPECT).abs() < 1e-3);
            assert!(area.expand(0.01).contains_rect(*r));
            for other in &rects[i + 1..] {
                assert!(r.intersect(*other).area() < 1e-3);
            }
        }
    }
}
