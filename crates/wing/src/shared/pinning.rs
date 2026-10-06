//! 会话置顶（pin）—— **前端约定，后端零感知**。
//!
//! pin 就是一个普通标签（[`PIN_TAG`]）：后端照常存、照常回，不认识它是
//! 什么意思（它只通用地记录「这个标签什么时候被加上的」——
//! `tag_meta[tag].added_at`）。语义全部发生在读到的这一侧：哪些会话被置顶、
//! 置顶的排在前面、后 pin 的排得更前。
//!
//! 排序规则的**唯一实现**在这里，TUI 的面板与 `wing ps` 共用（两处各自
//! 重写同一条规则就是漂移的开始）。它是一条稳定排序的键：
//!
//! ```text
//! 升序 = [ 置顶的（后 pin 的在前，时间未知的排已知之后）, 未置顶的（原序） ]
//! ```
//!
//! **刻意只有一条交互路径**：pin / unpin 都只能对「你所在的会话」做——状态栏
//! 的星标（`☆` 点亮 / `★` 熄灭）。给别的会话打 pin 走通用的
//! `wing tag <sid> pin`（幂等同理）；面板里的星标是**只读**的展示。这样一次
//! 误点永远不会摘掉别人（或你不在场的会话）的 pin——不是遗漏，是设计。
//!
//! 「时间未知」不是错误，但**不是**常态：写路径（状态栏星标、`wing tag`、
//! 创建即打标）都会记下时间，未知只来自三种脏源——打标时间上线之前就存在的
//! 老标签、手改 `metadata.json`、记录为空或不可解析。它们照常置顶，只是组内
//! 排在有时间者之后。

use std::cmp::Reverse;
use std::collections::HashMap;

use wing_api_client::models::TagMeta;

/// 置顶标签名（裸 tag：不携带时间，时间在通用标签记录里）。
pub const PIN_TAG: &str = "pin";

/// 该会话是否被置顶。
pub fn is_pinned(tags: &[String]) -> bool {
    tags.iter().any(|tag| tag == PIN_TAG)
}

/// 置顶时间（`tag_meta[PIN_TAG].added_at`；无记录 / 无时间 = `None`）。
pub fn pin_added_at(tag_meta: &HashMap<String, TagMeta>) -> Option<&str> {
    tag_meta.get(PIN_TAG)?.added_at.as_deref()
}

/// pin 排序键 —— 升序排列即目标顺序（配 `sort_by_cached_key` 使用，
/// 稳定排序：未置顶的条目键相等，相对顺序原样保留）。
///
/// 时间键用微秒比较；本地 naive 与带时区（RFC3339）两种写法都认，
/// 不可解析按未知处理（排在已知之后，而不是炸掉列表）。
pub fn pin_key(pinned: bool, added_at: Option<&str>) -> (bool, Reverse<Option<i64>>) {
    // `!pinned`：false < true，置顶的（`!pinned == false`）排在前面。
    // 时间只在置顶组内说话：未置顶的键恒等，靠稳定排序保持原序
    // （否则一条手写的旧记录会悄悄改写"非置顶"一栏的顺序）。
    let recency = if pinned {
        added_at.and_then(parse_added_at)
    } else {
        None
    };
    (!pinned, Reverse(recency))
}

/// ISO 时间 → 微秒（只用于**定序**：naive 值按 UTC 解释与同格式值比较，
/// 混合时区是既有时间口径的已知边界，与后端 `_timestamp_key` 的说明一致）。
fn parse_added_at(value: &str) -> Option<i64> {
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().timestamp_micros());
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_micros())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(entries: &[(&str, Option<&str>)]) -> HashMap<String, TagMeta> {
        entries
            .iter()
            .map(|(tag, at)| {
                (
                    (*tag).to_string(),
                    TagMeta {
                        added_at: at.map(str::to_string),
                    },
                )
            })
            .collect()
    }

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn pinned_is_the_bare_tag() {
        assert!(is_pinned(&tags(&["pin"])));
        assert!(is_pinned(&tags(&["task=x", "pin"])));
        assert!(!is_pinned(&tags(&["pinned"]))); // 不做前缀 / 语义匹配
        assert!(!is_pinned(&[]));
    }

    #[test]
    fn added_at_comes_from_the_generic_tag_record() {
        let records = meta(&[("pin", Some("2026-10-05T21:30:12.000001"))]);
        assert_eq!(pin_added_at(&records), Some("2026-10-05T21:30:12.000001"));
        // 记录缺失 / 时间为空 = 未知（不是错误）。
        assert_eq!(pin_added_at(&meta(&[])), None);
        assert_eq!(pin_added_at(&meta(&[("pin", None)])), None);
    }

    #[test]
    fn pinned_sorts_first_and_newer_pin_sorts_earlier() {
        let mut rows = [
            ("a-unpinned", false, None),
            ("b-old-pin", true, Some("2026-10-05T09:00:00")),
            ("c-unpinned", false, None),
            ("d-new-pin", true, Some("2026-10-06T09:00:00")),
        ];
        rows.sort_by_cached_key(|(_, pinned, at)| pin_key(*pinned, *at));
        let order: Vec<&str> = rows.iter().map(|(name, ..)| *name).collect();
        assert_eq!(
            order,
            ["d-new-pin", "b-old-pin", "a-unpinned", "c-unpinned"]
        );
    }

    #[test]
    fn unknown_time_pins_sort_after_timed_pins() {
        let mut rows = [
            ("unknown", true, None),
            ("timed", true, Some("2026-10-05T09:00:00")),
            ("garbage", true, Some("not-a-time")),
        ];
        rows.sort_by_cached_key(|(_, pinned, at)| pin_key(*pinned, *at));
        let order: Vec<&str> = rows.iter().map(|(name, ..)| *name).collect();
        // 有时间的在前；未知与不可解析都排在后面，且互相保持原序（稳定）。
        assert_eq!(order, ["timed", "unknown", "garbage"]);
    }

    #[test]
    fn unpinned_rows_keep_their_original_order() {
        let mut rows = [
            ("first", false, None),
            ("second", false, Some("2026-10-05T09:00:00")),
            ("third", false, None),
        ];
        rows.sort_by_cached_key(|(_, pinned, at)| pin_key(*pinned, *at));
        let order: Vec<&str> = rows.iter().map(|(name, ..)| *name).collect();
        assert_eq!(order, ["first", "second", "third"]);
    }

    #[test]
    fn rfc3339_timestamps_compare_by_instant() {
        let mut rows = [
            ("early", true, Some("2026-10-05T09:00:00+08:00")),
            ("late", true, Some("2026-10-05T03:00:00Z")),
        ];
        rows.sort_by_cached_key(|(_, pinned, at)| pin_key(*pinned, *at));
        let order: Vec<&str> = rows.iter().map(|(name, ..)| *name).collect();
        // 03:00Z = 11:00+08:00，晚于 09:00+08:00。
        assert_eq!(order, ["late", "early"]);
    }
}
