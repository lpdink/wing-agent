//! 数字格式化：状态栏 / CLI 表格 / 图片标签共用的纯函数。
//!
//! 共用的只有「数学与记法」；**呈现**（小数位、有无 `%`、着色）留在各自
//! 的消费点——状态栏要短（整数百分比），退出摘要有余量（一位小数）。这一
//! 条不是洁癖：两份独立实现已经漂移过一次（一处取整、一处小数），而口径
//! 本身（token 记法、命中率分子分母）不该有两份。
//!
//! 图片标签同理：composer 的 chip 与消息里的标签行走**同一个** [`image_label`]，
//! 否则「草稿里看到的」与「发出去/重放出来的」迟早会对不上。

/// 「没有读数」的唯一记号（不写 0：0 是一个读数，`—` 是"没有读数"）。
///
/// CLI 与将来的面板（状态栏 / 退出摘要）共用这一处：同一句「没有」写两遍就会有两种字形。
pub const NONE_MARK: &str = "—";

/// 千分位计数（`1,181` / `7,516`）：计数列的可扫读性全靠它。
pub fn count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// 一档比一档大的记法阶梯：`(进率, 后缀)`。
const SCALES: [(u64, char); 4] = [
    (1_000, 'k'),
    (1_000_000, 'M'),
    (1_000_000_000, 'G'),
    (1_000_000_000_000, 'T'),
];

/// 数量落到哪一档：返回 `(档位下标, 该档下的值)`。
fn scale_of(n: u64) -> (usize, f64) {
    let mut index = 0;
    for (i, (unit, _)) in SCALES.iter().enumerate() {
        if n >= *unit {
            index = i;
        }
    }
    let (unit, _) = SCALES[index];
    (index, n as f64 / unit as f64)
}

/// token 数的紧凑记法：`1.2k` / `3.4M` / `1.4G`（小数量级原样）。
///
/// G 档不是装饰：真实的窗口输入是上下文重发主导的（一台机器 4 天 1.36B prompt），
/// 没有它这一列会印成 `1360.0M`。舍入走档的规则同 [`tokens_capacity`]——差一点到
/// 下一档的读数不许读成 `1000.0M`。
///
/// 负数退化成裸数字（调用方要自带符号：`wing cache` 的 `≈-` 就是这么拼的）。
pub fn tokens(n: i64) -> String {
    if n < SCALES[0].0 as i64 {
        return n.to_string();
    }
    let (mut index, mut value) = scale_of(n as u64);
    loop {
        // 一位小数的记法会把 999.96 印成 `1000.0` —— 走一档（`999_960_000` → `1.0G`）。
        if value >= 999.95 && index + 1 < SCALES.len() {
            index += 1;
            value /= 1_000.0;
            continue;
        }
        return format!("{value:.1}{}", SCALES[index].1);
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
    if n < SCALES[0].0 {
        return n.to_string();
    }
    let (mut index, mut value) = scale_of(n);
    loop {
        // 记法会舍入，差一点到下一档的数会读成 `1000k` —— 走一档（`999_950` → `1M`）。
        if value.round() >= 1_000.0 && index + 1 < SCALES.len() {
            index += 1;
            value /= 1_000.0;
            continue;
        }
        let suffix = SCALES[index].1;
        return if (value - value.round()).abs() < 0.05 {
            format!("{:.0}{suffix}", value.round())
        } else {
            format!("{value:.1}{suffix}")
        };
    }
}

/// 工作量记法：`20h 00m` / `34m` / `12s`（毫秒进、人类记法出）。
///
/// 与两处**墙钟**记法分工不同，别混用：`cmd/exit_summary.rs::fmt_duration`（会话墙钟，
/// 分钟档带秒）与 `cmd/status.rs`（网关 uptime，小时档不带补齐的 00 分）。工时是账本
/// 里的累计毫秒，只到分钟。
pub fn work_time(ms: i64) -> String {
    let secs = ms.max(0) / 1_000;
    let (hours, minutes) = (secs / 3_600, (secs % 3_600) / 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
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
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_181), "1,181");
        assert_eq!(count(7_516), "7,516");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(-4_200), "-4,200");
    }

    #[test]
    fn work_time_stops_at_the_minute() {
        assert_eq!(work_time(72_000_000), "20h 00m");
        assert_eq!(work_time(2_040_000), "34m");
        assert_eq!(work_time(12_000), "12s");
        assert_eq!(work_time(-5), "0s", "负数没有意义，不印负号");
    }

    #[test]
    fn tokens_use_compact_notation_at_each_scale() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_200), "1.2k");
        assert_eq!(tokens(120_500), "120.5k");
        assert_eq!(tokens(3_400_000), "3.4M");
        // G 档：窗口输入是「亿」级的（上下文重发主导），`1360.0M` 读不了。
        assert_eq!(tokens(1_360_000_000), "1.4G");
        assert_eq!(tokens(1_000_000_000), "1.0G");
        assert_eq!(tokens(45_600_000_000), "45.6G");
        // 舍入不许把读数推进下一位数：差一点到 1G 的报 `1.0G`，不是 `1000.0M`。
        assert_eq!(tokens(999_960_000), "1.0G");
        assert_eq!(tokens(999_949_999), "999.9M");
        // T 档同理（再往上没有更大的档了，`1000.0T` 是最后一档的形态）。
        assert_eq!(tokens(1_000_000_000_000), "1.0T");
        assert_eq!(tokens(999_995_000_000), "1.0T");
        // 负数退化成裸数字（符号由调用方拼）。
        assert_eq!(tokens(-14_200), "-14200");
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
