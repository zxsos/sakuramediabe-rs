//! 竖封面书脊检测 —— 复刻 `movie_image_service.py:_detect_split_points`。
//!
//! 上游逻辑（`src/service/catalog/movie_image_service.py:100-146`）：
//!
//! 1. BGR → 灰度（`cv2.COLOR_BGR2GRAY`）
//! 2. `cv2.Sobel(gray, CV_64F, 1, 0, ksize=3)` 求水平方向梯度
//! 3. 取绝对值后按**列**求和，再除以最大值归一化
//! 4. 在中心两侧各 `center_range` 列内各取一个最大梯度列
//! 5. 先按「左右近似对称」或「右侧 ≈ 20 列」接受；否则按裁出区域的
//!    宽高比与边缘强度走三条保守规则，全不中则返回 `(-1, -1)`
//!
//! # 为什么值得照搬而不是重写
//!
//! 这个函数的输出决定**已入库封面是否被二次裁切**。规则里 `0.45..=0.85`、
//! `0.35`、`0.50` 这些阈值是从真实封面上调出来的，改动会让一批封面被误切
//! 或漏切。所以这里逐条对齐，包括 `(-1, -1)` 这个哨兵值。
//!
//! # 归一化抵消了哪些差异
//!
//! 第 3 步除以最大值，因此 Sobel 核的整体缩放（OpenCV 是否除以 8）**不影响
//! 结果** —— 只有核内权重比例（1:2:1）和灰度系数会影响。这正是下面
//! [`to_gray`] 那条待对拍项的由来。

/// 中心单侧搜索范围，对应上游默认参数 `center_range: int = 100`。
pub const DEFAULT_CENTER_RANGE: usize = 100;

/// RGB8 → 灰度。
///
/// 用 OpenCV **文档**给出的系数 `Y = 0.299R + 0.587G + 0.114B`。
///
/// ⚠️ **待对拍**：OpenCV 内部是 8 位定点实现（`Y = (77R + 150G + 29B + 128) >> 8`），
/// 与浮点公式在个别像素上可能差 1。列梯度最终按最大值归一化，该差异被压到
/// 0.4% 以下，分割点通常不受影响 —— 但必须用真实封面与 OpenCV 逐像素对拍确认。
/// 当前环境无 pip（装不了 `opencv-python`），尚未验证。
///
/// 输入按 3 字节一组处理，长度不是 3 的倍数时尾部不足一组的字节被忽略
/// （`chunks_exact`），不会 panic。
pub fn to_gray(rgb: &[u8]) -> Vec<f64> {
    rgb.chunks_exact(3)
        .map(|px| 0.299 * f64::from(px[0]) + 0.587 * f64::from(px[1]) + 0.114 * f64::from(px[2]))
        .collect()
}

/// `cv2.Sobel(gray, CV_64F, 1, 0, ksize=3)` 的逐列强度和，按最大值归一化。
///
/// 上游在归一化前对梯度取了绝对值再按列求和 —— 不取绝对值的话，一列的
/// 正负极值会互相抵消，纯色边界反而测不出来。
///
/// 返回长度 `width`、值域 `[0, 1]` 的向量；全平图像返回全 0（上游此时
/// `max_gradient == 0`，走 `(-1, -1)` 分支）。
pub fn column_gradient(gray: &[f64], width: usize, height: usize) -> Vec<f64> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut column = vec![0.0f64; width];
    for y in 0..height {
        for x in 0..width {
            let (x, y) = (x as i64, y as i64);
            // Gx 核（OpenCV 用的是相关而非卷积，不翻转核）：
            //   [-1  0  1]
            //   [-2  0  2]
            //   [-1  0  1]
            let right = at(gray, width, height, x + 1, y - 1)
                + 2.0 * at(gray, width, height, x + 1, y)
                + at(gray, width, height, x + 1, y + 1);
            let left = at(gray, width, height, x - 1, y - 1)
                + 2.0 * at(gray, width, height, x - 1, y)
                + at(gray, width, height, x - 1, y + 1);
            column[x as usize] += (right - left).abs();
        }
    }

    let max = column.iter().copied().fold(0.0, f64::max);
    if max > 0.0 {
        for v in &mut column {
            *v /= max;
        }
    }
    column
}

/// 复刻上游 `_detect_split_points`。
///
/// 返回 `(left, right)` 两个列号；未命中返回 `(-1, -1)` —— 这个哨兵值是
/// 上游 `_split_image` 的判断依据（`:176`），不能用 `Option` 替代。
pub fn detect_split_points(
    width: u32,
    height: u32,
    gray: &[f64],
    center_range: usize,
) -> (i32, i32) {
    let (width, height) = (width as usize, height as usize);
    if width == 0 || height == 0 {
        return (-1, -1);
    }
    // 上游不会传入长度不符的灰度图，但这个函数最终会被 HTTP 层调用 ——
    // 长度不符时 panic 会把整个请求线程带走，返回哨兵只是少切一次封面。
    if gray.len() < width * height {
        return (-1, -1);
    }

    let gradient = column_gradient(gray, width, height);
    if gradient.iter().copied().fold(0.0, f64::max) <= 0.0 {
        return (-1, -1);
    }

    let center_x = width / 2;
    let left_range = center_x.saturating_sub(center_range)..center_x;
    let right_range = center_x..(center_x + center_range).min(width);
    if left_range.is_empty() || right_range.is_empty() {
        return (-1, -1);
    }

    // Python 的 max(range, key=...) 取**首个**最大值，这里必须一致：
    // Rust 的 Iterator::max_by 在相等时返回后者，会把分割点整体右移一列。
    let left_point = argmax(&gradient, left_range);
    let right_point = argmax(&gradient, right_range);

    let left_distance = center_x.abs_diff(left_point);
    let right_distance = right_point.abs_diff(center_x);

    if left_distance.abs_diff(right_distance) < 10 || right_distance.abs_diff(20) < 10 {
        return (left_point as i32, right_point as i32);
    }

    let crop_aspect_ratio = (width - right_point) as f64 / height as f64;
    let right_edge_strength = gradient[right_point];

    // 三条保守规则：宽书脊 / 窄书脊 / 正好从中心切
    let wide_spine = (12..=center_range).contains(&right_distance)
        && (0.45..=0.85).contains(&crop_aspect_ratio)
        && right_edge_strength >= 0.35;
    let narrow_spine = (4..=11).contains(&right_distance)
        && (0.55..=0.80).contains(&crop_aspect_ratio)
        && right_edge_strength >= 0.50;
    let center_split = right_distance == 0
        && (0.55..=0.80).contains(&crop_aspect_ratio)
        && right_edge_strength >= 0.50;

    if !(wide_spine || narrow_spine || center_split) {
        return (-1, -1);
    }
    (left_point as i32, right_point as i32)
}

/// 取区间内**首个**最大值下标（对齐 Python `max(range, key=...)` 的语义）。
fn argmax(values: &[f64], range: std::ops::Range<usize>) -> usize {
    let mut best = range.start;
    let mut best_value = f64::NEG_INFINITY;
    for i in range {
        if values[i] > best_value {
            best_value = values[i];
            best = i;
        }
    }
    best
}

/// `BORDER_REFLECT_101` 取样 —— `cv2.Sobel` 的默认边界模式。
///
/// 上游没显式指定 `borderType`，用的是 `BORDER_DEFAULT`(=`REFLECT_101`)：
/// `-1 → 1`、`width → width-2`。用零填充会在图像左右边缘各造出一条假边界，
/// 而封面书脊常常就在边缘。
fn at(gray: &[f64], width: usize, height: usize, x: i64, y: i64) -> f64 {
    let cx = reflect101(x, width as i64);
    let cy = reflect101(y, height as i64);
    gray[cy * width + cx]
}

fn reflect101(index: i64, len: i64) -> usize {
    if len <= 1 {
        return 0;
    }
    let mut i = index;
    if i < 0 {
        i = -i;
    }
    if i >= len {
        i = 2 * len - 2 - i;
    }
    i.clamp(0, len - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造列恒定灰度图：每个 x 给一个灰度值，整列相同。
    fn columns(width: usize, height: usize, f: impl Fn(usize) -> f64) -> Vec<f64> {
        let mut gray = Vec::with_capacity(width * height);
        for _y in 0..height {
            for x in 0..width {
                gray.push(f(x));
            }
        }
        gray
    }

    #[test]
    fn gray_uses_the_documented_opencv_coefficients() {
        let gray = to_gray(&[255, 255, 255, 0, 0, 0, 255, 0, 0]);
        assert!((gray[0] - 255.0).abs() < 1e-9, "白色 -> 255");
        assert!(gray[1].abs() < 1e-9, "黑色 -> 0");
        assert!((gray[2] - 0.299 * 255.0).abs() < 1e-9, "纯红只保留 R 分量");
    }

    #[test]
    fn gradient_peaks_exactly_at_the_step() {
        // 左全黑右全白：梯度只应出现在分界列的两侧
        let gray = columns(200, 100, |x| if x < 100 { 0.0 } else { 255.0 });
        let grad = column_gradient(&gray, 200, 100);

        assert_eq!(grad.len(), 200);
        assert!((grad[99] - 1.0).abs() < 1e-9, "分界左侧列应为峰值");
        assert!((grad[100] - 1.0).abs() < 1e-9, "分界右侧列应为峰值");
        assert!(grad[98] < 1e-9, "再往外一列应无梯度，实得 {}", grad[98]);
        assert!(grad[101] < 1e-9, "再往外一列应无梯度，实得 {}", grad[101]);
    }

    #[test]
    fn a_centered_step_is_detected_as_the_split_pair() {
        let gray = columns(200, 100, |x| if x < 100 { 0.0 } else { 255.0 });
        assert_eq!(
            detect_split_points(200, 100, &gray, DEFAULT_CENTER_RANGE),
            (99, 100)
        );
    }

    #[test]
    fn a_flat_image_has_no_split_point() {
        let gray = columns(200, 100, |_| 128.0);
        assert_eq!(
            detect_split_points(200, 100, &gray, DEFAULT_CENTER_RANGE),
            (-1, -1)
        );
    }

    #[test]
    fn the_center_split_rule_accepts_a_strong_center_edge() {
        // 中心处（x=200）有一道强边，左侧 x=110 处还有一道更强的边：
        // 左右距离差 91 不满足对称规则，只能靠 center_split_matched 接受。
        let gray = columns(400, 300, |x| {
            if x < 110 {
                0.0
            } else if x < 200 {
                100.0
            } else {
                160.0
            }
        });
        let (left, right) = detect_split_points(400, 300, &gray, DEFAULT_CENTER_RANGE);
        assert_eq!(right, 200, "右分割点应落在中心强边上");
        assert_eq!(left, 109, "左分割点应落在 x=110 那道边的左列");
    }

    #[test]
    fn a_wide_image_without_a_cover_shaped_crop_is_rejected() {
        // 1000x100 的横图，唯一强边在 x=580：裁出区域宽高比 4.2，三条规则全不匹配
        let gray = columns(1000, 100, |x| if x < 580 { 0.0 } else { 255.0 });
        assert_eq!(
            detect_split_points(1000, 100, &gray, DEFAULT_CENTER_RANGE),
            (-1, -1)
        );
    }

    #[test]
    fn empty_and_degenerate_inputs_return_the_sentinel() {
        assert_eq!(
            detect_split_points(0, 100, &[], DEFAULT_CENTER_RANGE),
            (-1, -1)
        );
        assert_eq!(
            detect_split_points(100, 0, &[], DEFAULT_CENTER_RANGE),
            (-1, -1)
        );
        // center_range=0 -> 左区间为空，这是唯一能让区间退化为空的输入
        let gray = columns(200, 100, |_| 1.0);
        assert_eq!(detect_split_points(200, 100, &gray, 0), (-1, -1));
        // 灰度图长度与 width*height 不符时不得 panic
        assert_eq!(detect_split_points(200, 100, &[1.0; 10], 0), (-1, -1));
    }

    #[test]
    fn a_degenerate_width_still_returns_a_pair_like_upstream() {
        // 宽度远小于 center_range 时区间**不会**退化为空（左区间从 0 起），
        // 上游 Python 同样是 range(0, 2)，所以这里也返回一对列号而非哨兵。
        // 这条断言的作用是钉住该行为 —— 别把它"顺手改成" (-1, -1)。
        let gray = columns(4, 4, |x| if x < 2 { 0.0 } else { 255.0 });
        assert_eq!(
            detect_split_points(4, 4, &gray, DEFAULT_CENTER_RANGE),
            (1, 2)
        );
    }

    #[test]
    fn reflect101_matches_the_opencv_border_mode() {
        assert_eq!(reflect101(-1, 8), 1, "REFLECT_101: -1 -> 1");
        assert_eq!(reflect101(8, 8), 6, "REFLECT_101: len -> len-2");
        assert_eq!(reflect101(3, 8), 3, "区间内原样返回");
        assert_eq!(reflect101(0, 1), 0, "长度为 1 时不越界");
    }
}
