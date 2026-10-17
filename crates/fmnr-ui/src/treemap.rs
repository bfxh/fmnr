//! squarify 铺法：把一组权重铺成**面积比 = 数值比、长宽比尽量接近 1** 的矩形。
//!
//! 为什么不是"一条一条切高度"：那样 100 个小目录会切成 100 根细条，
//! 谁大谁小只能靠读数字。占比条回答的是"这一项占百分之几"，
//! 而 treemap 回答的是"这一层的地盘谁占了哪一块"——后者才是看磁盘占用时想问的。
//!
//! 这里只有几何，不碰 egui：输入 `&[f64]` 和一块矩形，输出同样长度的一串矩形。
//! 所以它能脱离界面被量（面积对不对、有没有重叠、长宽比好不好）。

/// 一块矩形。用 `f64` 而不是 `egui::Rect`：这个模块不该为了画图而依赖 egui。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cell {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// 只有测试用得到的两把尺子。放在 `#[cfg(test)]` 里是因为产品代码确实不需要：
/// 画图只要矩形本身，"面积对不对、方不方"是断言在问的问题。
#[cfg(test)]
impl Cell {
    pub fn area(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }
    /// 长宽比（总是 ≥ 1）。1 = 正方形；越大连越长条。
    pub fn aspect(&self) -> f64 {
        let w = self.w.max(0.0);
        let h = self.h.max(0.0);
        if w <= f64::EPSILON || h <= f64::EPSILON {
            return 1.0;
        }
        (w / h).max(h / w)
    }
}

/// 把 `weights` 铺进 `w × h` 的矩形，返回**与输入同序**的一串矩形。
///
/// 权重为 0 的项拿不到面积（退化成一个空矩形），也不会把整张图除爆。
/// 输入不需要预先排序——内部按降序铺，但结果按下标写回。
pub fn squarify(weights: &[f64], w: f64, h: f64) -> Vec<Cell> {
    let mut out = vec![Cell::default(); weights.len()];
    if w <= 0.0 || h <= 0.0 {
        return out;
    }
    let mut items: Vec<(usize, f64)> = weights
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, v)| *v > 0.0 && v.is_finite())
        .collect();
    if items.is_empty() {
        return out;
    }
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let raw: f64 = items.iter().map(|(_, v)| v).sum();
    let target = w * h;
    let scaled: Vec<f64> = items.iter().map(|(_, v)| v / raw * target).collect();

    let mut rect = Cell {
        x: 0.0,
        y: 0.0,
        w,
        h,
    };
    let mut i = 0usize;
    while i < items.len() {
        let horizontal = rect.w >= rect.h;
        let side = if horizontal { rect.w } else { rect.h };
        if side <= f64::EPSILON {
            break;
        }
        // 贪心扩行：只要"最差长宽比"不再变差就一直加。
        let mut row: Vec<usize> = vec![i];
        let mut sum = scaled[i];
        let mut best = worst(&row, &scaled, side, sum);
        let mut j = i + 1;
        while j < items.len() {
            let cand_sum = sum + scaled[j];
            row.push(j);
            let cand = worst(&row, &scaled, side, cand_sum);
            if cand <= best {
                sum = cand_sum;
                best = cand;
                j += 1;
            } else {
                row.pop();
                break;
            }
        }
        // 最后一行把剩下的整条厚度吃掉：浮点误差不然会在边上留一条缝。
        let thick = if j >= items.len() {
            if horizontal {
                rect.h
            } else {
                rect.w
            }
        } else {
            sum / side
        };
        let mut off = 0.0;
        for &k in &row {
            let len = scaled[k] / thick.max(f64::EPSILON);
            out[items[k].0] = if horizontal {
                Cell {
                    x: rect.x + off,
                    y: rect.y,
                    w: len,
                    h: thick,
                }
            } else {
                Cell {
                    x: rect.x,
                    y: rect.y + off,
                    w: thick,
                    h: len,
                }
            };
            off += len;
        }
        rect = if horizontal {
            Cell {
                x: rect.x,
                y: rect.y + thick,
                w: rect.w,
                h: (rect.h - thick).max(0.0),
            }
        } else {
            Cell {
                x: rect.x + thick,
                y: rect.y,
                w: (rect.w - thick).max(0.0),
                h: rect.h,
            }
        };
        i = j;
    }
    out
}

/// 一行（沿长度为 `side` 的边铺）里**最差**那一格的长宽比。
///
/// Bruls 等人的原式：面积 r、行内面积和 s、铺的边长 w 时，
/// 每格是 `(r/s·w) × (s/w)`，其长宽比的上界取两端最坏的两个：
/// `max( w²·rmax / s² , s² / (w²·rmin) )`。
fn worst(row: &[usize], scaled: &[f64], side: f64, sum: f64) -> f64 {
    let (mn, mx) = row.iter().fold((f64::INFINITY, 0.0f64), |acc, &k| {
        (acc.0.min(scaled[k]), acc.1.max(scaled[k]))
    });
    let d2 = side * side;
    let s2 = sum * sum;
    let a = d2 * mx / s2;
    let b = if mn > f64::EPSILON {
        s2 / (d2 * mn)
    } else {
        f64::INFINITY
    };
    a.max(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum_area(cells: &[Cell]) -> f64 {
        cells.iter().map(|c| c.area()).sum()
    }

    /// 面积比必须等于权重比——这是 treemap 的全部承诺。
    /// 占比条改成方块图，如果面积不再忠实于数值，那就是把一张能读的图
    /// 换成一张好看的假图。
    #[test]
    fn cell_areas_are_proportional_to_weights() {
        let w = [1000.0f64, 500.0, 250.0, 250.0];
        let cells = squarify(&w, 200.0, 100.0);
        let total: f64 = w.iter().sum();
        for (i, c) in cells.iter().enumerate() {
            let want = w[i] / total * 200.0 * 100.0;
            assert!(
                (c.area() - want).abs() / want < 0.02,
                "第 {i} 项权重 {} 应占 {want}，实占 {}",
                w[i],
                c.area()
            );
        }
    }

    /// 铺满、不重叠：总面积等于矩形面积，且任意两格交集面积≈0。
    /// 这条钉的是"行厚度和偏移算错导致缝隙/叠压"——treemap 最常见的错法。
    #[test]
    fn cells_tile_the_box_without_gaps_or_overlap() {
        for weights in [
            vec![1.0, 1.0, 1.0],
            vec![9.0, 1.0, 1.0, 1.0, 1.0],
            vec![5000.0, 1.0, 1.0, 1.0],
            (0..37).map(|i| (i * i + 1) as f64).collect::<Vec<f64>>(),
        ] {
            let cells = squarify(&weights, 320.0, 180.0);
            let got = sum_area(&cells);
            let want = 320.0 * 180.0;
            assert!(
                (got - want).abs() / want < 0.01,
                "{weights:?} 铺完总面积 {got}，应约 {want}"
            );
            for a in 0..cells.len() {
                for b in (a + 1)..cells.len() {
                    let ix = (cells[a].x + cells[a].w).min(cells[b].x + cells[b].w)
                        - cells[a].x.max(cells[b].x);
                    let iy = (cells[a].y + cells[a].h).min(cells[b].y + cells[b].h)
                        - cells[a].y.max(cells[b].y);
                    let overlap = ix.max(0.0) * iy.max(0.0);
                    assert!(
                        overlap < 1.0,
                        "第 {a} 与第 {b} 格重叠 {overlap}：{:?} vs {:?}",
                        cells[a],
                        cells[b]
                    );
                }
            }
        }
    }

    /// **这条才是"非线性"的判据**：几十个小项不许被切成几十根细条。
    /// 用切片式（slice-and-dice）画法时，100 个等权项在 100×100 里会变成
    /// 100 根 1×100 的条，长宽比 100；squarify 应该在个位数以内。
    #[test]
    fn many_small_items_stay_near_square_instead_of_becoming_slivers() {
        let weights = vec![1.0f64; 100];
        let cells = squarify(&weights, 100.0, 100.0);
        let worst = cells.iter().map(|c| c.aspect()).fold(0.0f64, f64::max);
        assert!(
            worst <= 3.0,
            "100 个等权项里最差长宽比 {worst}，细条化了（切片式画法会到 100）"
        );
    }

    /// 权重差一个数量级时也要接近正方形，而不是让小的变成缝里的细条。
    #[test]
    fn skewed_weights_do_not_degenerate_into_slivers() {
        let mut weights = vec![1000.0f64];
        weights.extend((0..20).map(|_| 1.0));
        let cells = squarify(&weights, 400.0, 200.0);
        let worst = cells[1..].iter().map(|c| c.aspect()).fold(0.0f64, f64::max);
        assert!(worst <= 6.0, "20 个小项里最差长宽比 {worst}");
    }

    /// 0 权重（空目录）拿不到面积，也不能把整张图 NaN 掉。
    /// 实测这条不是防御性代码：扫描结果里空目录是真的会出现。
    #[test]
    fn zero_weight_cells_get_no_area_and_do_not_poison_the_layout() {
        let weights = [0.0f64, 500.0, 0.0, 500.0];
        let cells = squarify(&weights, 100.0, 100.0);
        assert_eq!(cells[0].area(), 0.0);
        assert_eq!(cells[2].area(), 0.0);
        assert!(cells[1].area() > 0.0 && cells[3].area() > 0.0);
        assert!(
            cells.iter().all(|c| c.area().is_finite()),
            "出现非有限面积：{cells:?}"
        );
    }

    /// 退化输入不许 panic：空表、零尺寸、以及"全是 0"。
    #[test]
    fn degenerate_inputs_return_empty_layouts_without_panicking() {
        assert!(squarify(&[], 100.0, 100.0).is_empty());
        assert_eq!(squarify(&[1.0, 2.0], 0.0, 100.0).len(), 2);
        assert!(squarify(&[1.0, 2.0], 0.0, 100.0)
            .iter()
            .all(|c| c.area() == 0.0));
        let all_zero = squarify(&[0.0, 0.0], 50.0, 50.0);
        assert_eq!(all_zero.len(), 2);
        assert!(all_zero.iter().all(|c| c.area() == 0.0));
    }

    /// 返回顺序**必须**和输入下标对齐：调用方拿 `buckets[i]` 配 `cells[i]`。
    /// 内部按降序铺，如果忘了写回原位，标签就会贴到别人的方块上——
    /// 而这张图看起来完全正常，只有读数字的人会被骗。
    #[test]
    fn output_is_indexed_by_input_not_by_layout_order() {
        let weights = [1.0f64, 900.0, 2.0];
        let cells = squarify(&weights, 300.0, 100.0);
        assert!(
            cells[1].area() > cells[0].area() && cells[1].area() > cells[2].area(),
            "第 1 项权重最大却拿不到最大格：{:?}",
            cells.iter().map(|c| c.area()).collect::<Vec<_>>()
        );
        // 反证：如果实现按降序直接吐格子，cells[0] 会是最大那块。
        assert!(
            cells[0].area() < cells[1].area(),
            "输出像是按降序排的，下标对齐已经破了"
        );
    }
}
