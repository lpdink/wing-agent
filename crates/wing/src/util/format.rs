//! 数字格式化：状态栏与 CLI 表格共用的纯函数。
//!
//! 共用的只有「数学与记法」；**呈现**（小数位、有无 `%`、着色）留在各自
//! 的消费点——状态栏要短（整数百分比），退出摘要有余量（一位小数）。这一
//! 条不是洁癖：两份独立实现已经漂移过一次（一处取整、一处小数），而口径
//! 本身（token 记法、命中率分子分母）不该有两份。

/// token 数的紧凑记法：`1.2k` / `3.4M`（小数量级原样）。
pub fn tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// 缓存命中率（百分比，0..=100）。没有可命中的输入（`prompt <= 0`）→ `None`
/// （呈现层决定怎么表示"无"：状态栏不显示、摘要用 `-`）。
pub fn hit_rate(cached: i64, prompt: i64) -> Option<f64> {
    if prompt <= 0 {
        return None;
    }
    Some(cached.max(0) as f64 / prompt as f64 * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_use_compact_notation_at_each_scale() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_200), "1.2k");
        assert_eq!(tokens(120_500), "120.5k");
        assert_eq!(tokens(3_400_000), "3.4M");
    }

    #[test]
    fn hit_rate_needs_a_non_zero_denominator() {
        assert_eq!(hit_rate(0, 0), None);
        assert_eq!(hit_rate(5, 10), Some(50.0));
        // 摘要在意的是一位小数（浮点位模式不是契约）。
        let rate = hit_rate(80_000, 120_500).expect("non-zero denominator");
        assert_eq!(format!("{rate:.1}%"), "66.4%");
    }
}
