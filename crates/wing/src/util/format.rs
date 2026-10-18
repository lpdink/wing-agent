//! 数字格式化：状态栏 / CLI 表格 / 图片标签共用的纯函数。
//!
//! 共用的只有「数学与记法」；**呈现**（小数位、有无 `%`、着色）留在各自
//! 的消费点——状态栏要短（整数百分比），退出摘要有余量（一位小数）。这一
//! 条不是洁癖：两份独立实现已经漂移过一次（一处取整、一处小数），而口径
//! 本身（token 记法、命中率分子分母）不该有两份。
//!
//! 图片标签同理：composer 的 chip 与消息里的标签行走**同一个** [`image_label`]，
//! 否则「草稿里看到的」与「发出去/重放出来的」迟早会对不上。

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

/// 容量记法：整数位（`500k` / `1M`）—— 给"分母"用。
///
/// 窗口大小天生是整数量级，`500.0k` 里的 `.0` 只让人以为量到了个位；而
/// `0 / 500.0k` 这种读数在状态栏上像一块仪表。已用量仍走 [`tokens`]（那位小数
/// 是真的在动）。
pub fn tokens_round(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.0}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
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

/// 一张图片的标签：`[Image #N W×H SIZE]`——composer 的图片 chip 与消息里的图片
/// 标签行共用的记法（两处各写一份必然漂移）。
///
/// `number` 的口径由调用方定：chip 用**草稿内**号码（05 的契约：单调不复用），消息
/// 用**消息内** 1-based 序号（重放只有媒体列表，没有草稿号码）。`W×H` 与 `SIZE`
/// 取的都是真正发出去的那份产物（压缩后的字节数与尺寸）。
pub fn image_label(number: usize, width: u32, height: u32, bytes: u64) -> String {
    format!("[Image #{number} {width}×{height} {}]", image_bytes(bytes))
}

/// 图片字节数的紧凑记法：`512B` / `384KB` / `1.0MB`。
///
/// KB 四舍五入到整数，且**永不落成 `1024KB`**——那读起来该是 `1.0MB`。
///
/// 也用在**在途** chip 的进度文案上（`[Image #1 ⠋ ↓12.4MB]`，见
/// `ui/input_area/image.rs`）：同一份记法读的是「这张图多大」，无论是已经压好的
/// 产物还是正在传来的字节。
pub(crate) fn image_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes < KIB {
        format!("{bytes}B")
    } else if bytes + KIB / 2 < MIB {
        format!("{}KB", (bytes + KIB / 2) / KIB)
    } else {
        format!("{:.1}MB", bytes as f64 / MIB as f64)
    }
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

    #[test]
    fn the_image_label_carries_the_number_the_size_and_the_bytes() {
        assert_eq!(
            image_label(1, 1024, 768, 393_216),
            "[Image #1 1024×768 384KB]"
        );
        assert_eq!(
            image_label(12, 800, 600, 1_048_576),
            "[Image #12 800×600 1.0MB]"
        );
        assert_eq!(image_label(3, 8, 8, 7), "[Image #3 8×8 7B]");
    }

    #[test]
    fn the_byte_notation_covers_its_boundaries() {
        assert_eq!(image_bytes(0), "0B");
        assert_eq!(image_bytes(7), "7B");
        assert_eq!(image_bytes(1023), "1023B");
        assert_eq!(image_bytes(1024), "1KB");
        assert_eq!(image_bytes(1536), "2KB", "rounded to the nearest KB");
        // 非整除：四舍五入，不是向上取整（ceil 会给 2KB / 3KB）。
        assert_eq!(image_bytes(1025), "1KB");
        assert_eq!(image_bytes(2047), "2KB");
        assert_eq!(image_bytes(393_216), "384KB");
        // Just under a mebibyte rounds to 1024KB *only* if that were allowed —
        // the branch hands it to MB instead, so the label never reads `1024KB`.
        assert_eq!(image_bytes(1_048_575), "1.0MB");
        assert_eq!(image_bytes(1_048_576), "1.0MB");
        assert_eq!(image_bytes(1_572_864), "1.5MB");
    }
}
