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

/// 容量记法：`128k` / `500k` / `1M` / `1.5M` —— 给"上限"用（上下文窗口、保留量）。
///
/// 与 [`tokens`] 的差别是**可扫读**：整数量级不写 `.0`（`500k` 而不是 `500.0k`，
/// 那位小数只让人以为量到了个位），非整数保留一位。舍入不许把读数推进下一档的
/// 位数 —— `999_950` 报 `1M` 而不是 `1000k`（走档）。
///
/// 记法只有一份：model picker 的 ctx 列与状态栏的 `0 / 500k` 都读它，否则同一个
/// 窗口会在两处显示成两个数。
pub fn tokens_capacity(n: u64) -> String {
    const SCALES: [(u64, char); 3] = [(1_000, 'k'), (1_000_000, 'M'), (1_000_000_000, 'G')];
    if n < SCALES[0].0 {
        return n.to_string();
    }
    let mut index = 0;
    for (i, (unit, _)) in SCALES.iter().enumerate() {
        if n >= *unit {
            index = i;
        }
    }
    loop {
        let (unit, suffix) = SCALES[index];
        let value = n as f64 / unit as f64;
        // 记法会舍入，差一点到下一档的数会读成 `1000k` —— 走一档（`999_950` → `1M`）。
        if value.round() >= 1_000.0 && index + 1 < SCALES.len() {
            index += 1;
            continue;
        }
        return if (value - value.round()).abs() < 0.05 {
            format!("{:.0}{suffix}", value.round())
        } else {
            format!("{value:.1}{suffix}")
        };
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
/// `number` 的口径由调用方定：chip 用**草稿内**号码（单调不复用），消息
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
    fn the_capacity_notation_drops_the_zero_and_walks_a_scale() {
        assert_eq!(tokens_capacity(0), "0");
        assert_eq!(tokens_capacity(999), "999");
        assert_eq!(tokens_capacity(1_000), "1k");
        assert_eq!(tokens_capacity(128_000), "128k");
        assert_eq!(tokens_capacity(131_072), "131.1k");
        assert_eq!(tokens_capacity(500_000), "500k");
        assert_eq!(tokens_capacity(1_000_000), "1M");
        assert_eq!(tokens_capacity(1_500_000), "1.5M");
        // 舍入不许把读数推进下一档的位数：差一点到 1M 的报 `1M`，不是 `1000k`。
        assert_eq!(tokens_capacity(999_950), "1M");
        assert_eq!(tokens_capacity(999_999), "1M");
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
