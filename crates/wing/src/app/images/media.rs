//! 消息里的媒体图（网关对象）的**字节侧**：屏幕上要哪几张、谁在飞、字节缓存多大。
//!
//! 三层分工，各自的生命周期不同：
//!
//! ```text
//! 布局   ui::chat_view::media       形状来自 MediaRef（零 I/O），盒高与行号纯函数
//! 字节   本模块（app::images::media）随视口来去：滚出屏幕就该还内存
//! 编码   ui::image::ImageStore      按 (id, target, cell) 缓存，随终端状态作废
//! ```
//!
//! **只抓屏幕上的**：`observe` 每帧用这一帧的 `FrameImage` 重算目标集（与新鲜度 lane 的
//! `watch` 同一来源、同一种「由屏高封顶」的有界性）。打开一个 50 张图的老会话不会立刻发
//! 50 个 GET；滚到哪抓到哪。内容寻址保证抓到的永远同一份字节，所以「滚出去丢掉、滚回来
//! 重抓」与一直留着是等价的，只是内存更省。
//!
//! **字节缓存的边界**：在屏的图不受张数目标约束（同屏超过目标时，纯 LRU 会变成每帧
//! 重抓的自激环），离屏的按最久未用淘汰；`BYTE_BUDGET` 是硬上限——真要在屏的图也装不下
//! 时，被挤掉的那张在这一代内容里不再要（否则同样是每帧重抓的自激环），屏幕上看是它退回
//! caption，滚出去再滚回来就恢复。
//!
//! **失败是一种状态**：抓不到（对象缺失 / 归档 / 网关不可达）记在 `failed` 里，本代内容内
//! 不再自动重试——气泡里那一张保持 caption（不空白、不闪）。它会在图片**离开视口**时被
//! 忘掉（再滚回来就再试一次），内容重建（切会话 / 压缩 / rewind）也会整表清空。

use std::collections::HashSet;
use std::sync::Arc;

/// 同时在飞的抓取上限：滚过一串图时不会一次压出二十个请求。
pub(crate) const MAX_INFLIGHT: usize = 8;

/// 字节缓存的**软**张数目标（与编码 LRU 的 `DEFAULT_CACHE_ENTRIES` 同量级）。
///
/// 实际目标是 `max(BYTE_ENTRIES, 屏幕上的张数)`：同屏超过这个数时，纯 LRU 会形成
/// 「每帧挤掉一张在屏的图 → 下一帧重抓重编码」的自激环（屏幕上看是闪烁），而一屏的图
/// 本来就受屏高约束，不该被自己的数量挤掉。
pub(crate) const BYTE_ENTRIES: usize = 8;

/// 字节缓存的字节上限（**硬**上限：内存有界优先于避免重抓）。
///
/// 单张 ≤ 网关的 `images.max_bytes`（默认 4.5 MiB），8 张通常够不到这里；这条是给
/// 「网关放宽了上限 / 别的客户端传了大图 / 同屏图特别多」留的兜底。
pub(crate) const BYTE_BUDGET: usize = 24 * 1024 * 1024;

/// 失败记录的上限（整表丢弃，与 store 的 `failed` memo 同一策略）。
const MAX_FAILED: usize = 64;

/// 媒体图的字节缓存与抓取账本。默认（`disabled` lane）为空且每次调用都是空操作。
#[derive(Default)]
pub(crate) struct MediaLane {
    /// 这一帧屏幕上的媒体 id（去重，按出现序；随帧重算）。
    watch: Vec<String>,
    /// 已抓到的字节，index 0 = 最久未用。
    bytes: Vec<(String, Arc<Vec<u8>>)>,
    bytes_total: usize,
    /// 在飞的抓取（每个 id 至多一个请求；回执按 id 认领）。
    inflight: HashSet<String>,
    /// 「这一代内容里不再向网关要」：抓失败，或字节被预算挤出缓存（见
    /// [`MediaLane::evict_one`]）。离开视口时忘掉。
    failed: HashSet<String>,
}

impl MediaLane {
    /// 这一帧要画的媒体 id（`FrameImage` 的来源，已去重）。
    ///
    /// 顺手修剪失败记录：离开视口的失败不再记账，滚回来时重新试一次。
    pub(crate) fn observe(&mut self, wanted: impl IntoIterator<Item = String>) {
        self.watch.clear();
        for id in wanted {
            if !self.watch.contains(&id) {
                self.watch.push(id);
            }
        }
        let watch = &self.watch;
        self.failed.retain(|id| watch.contains(id));
    }

    /// 已抓到的字节（命中即刷新 LRU 位置）。
    pub(crate) fn bytes(&mut self, id: &str) -> Option<Arc<Vec<u8>>> {
        let index = self.bytes.iter().position(|(key, _)| key == id)?;
        let entry = self.bytes.remove(index);
        let bytes = Arc::clone(&entry.1);
        self.bytes.push(entry);
        Some(bytes)
    }

    /// 一张图抓回来了。
    pub(crate) fn supply(&mut self, id: &str, bytes: Arc<Vec<u8>>) {
        self.inflight.remove(id);
        self.failed.remove(id);
        if let Some(index) = self.bytes.iter().position(|(key, _)| key == id) {
            let (_, old) = self.bytes.remove(index);
            self.bytes_total = self.bytes_total.saturating_sub(old.len());
        }
        self.bytes_total = self.bytes_total.saturating_add(bytes.len());
        self.bytes.push((id.to_string(), bytes));
        // 张数：软目标（见 `BYTE_ENTRIES`），在屏的图不受它约束。
        let target = BYTE_ENTRIES.max(self.watch.len()).max(1);
        while self.bytes.len() > target {
            self.evict_one();
        }
        // 字节：硬上限。单张就超预算时也至少留下它自己（否则每一帧都要重抓重编码）。
        while self.bytes_total > BYTE_BUDGET && self.bytes.len() > 1 {
            self.evict_one();
        }
    }

    /// 挤掉一条字节缓存：**离屏的**优先（最久未用优先），全都在屏幕上时退化为纯 LRU。
    ///
    /// 被挤掉的**在屏**条目记成「不再要」：它只在字节预算（硬上限）下才会发生——不记的话
    /// 「挤出 → 下一帧重抓 → 回执把它挤回来」是一个不需要用户输入的自激环（同屏原始字节
    /// 大于预算时每帧一次 GET + 重编码）。屏幕上看：那张图退回 caption，滚出去再滚回来就好。
    fn evict_one(&mut self) {
        let index = self
            .bytes
            .iter()
            .position(|(key, _)| !self.watch.contains(key))
            .unwrap_or(0);
        let (victim, evicted) = self.bytes.remove(index);
        self.bytes_total = self.bytes_total.saturating_sub(evicted.len());
        if self.watch.contains(&victim) {
            self.record(&victim);
        }
    }

    /// 一张图抓失败了。
    pub(crate) fn fail(&mut self, id: &str) {
        self.inflight.remove(id);
        self.record(id);
    }

    /// 「这一代内容里不再向网关要这张」：抓失败，或它的字节被预算挤出了缓存（见
    /// [`MediaLane::evict_one`]）。离开视口时忘掉（[`MediaLane::observe`]），滚回来再试。
    fn record(&mut self, id: &str) {
        if self.failed.len() >= MAX_FAILED {
            self.failed.clear();
        }
        self.failed.insert(id.to_string());
    }

    /// 下一批要抓的 id（屏幕上的、没字节的、没在飞的、没失败过的），并把它们记成在飞。
    ///
    /// 上限是 [`MAX_INFLIGHT`]：剩下的留在屏幕上，等这一批的回执把位置腾出来——
    /// 每个回执都会让 App 重画，所以进度不会停。
    pub(crate) fn requests(&mut self) -> Vec<String> {
        if self.inflight.len() >= MAX_INFLIGHT {
            return Vec::new();
        }
        let mut out = Vec::new();
        for id in &self.watch {
            if self.inflight.len() + out.len() >= MAX_INFLIGHT {
                break;
            }
            if self.failed.contains(id) || self.inflight.contains(id) {
                continue;
            }
            if self.bytes.iter().any(|(key, _)| key == id) {
                continue;
            }
            out.push(id.clone());
        }
        for id in &out {
            self.inflight.insert(id.clone());
        }
        out
    }

    /// 内容重建（切会话 / 压缩 / rewind）：屏幕目标集与失败记录都归零，让下一帧重来。
    ///
    /// 字节**不丢**：内容是内容寻址的，同一份字节在重建之后依然有效（LRU 会自己淘汰）。
    /// 在飞集合也清掉——重建之后「还在飞」的那张要么已经落地（回执按 id 幂等），要么会在
    /// 下一次 `requests` 里重新问一次（内容寻址：重复抓同一份字节没有副作用）。
    pub(crate) fn forget_content(&mut self) {
        self.watch.clear();
        self.inflight.clear();
        self.failed.clear();
    }

    /// 一张媒体图的字节是否已经没有指望（诊断 / 测试）。
    #[cfg(test)]
    pub(crate) fn is_failed(&self, id: &str) -> bool {
        self.failed.contains(id)
    }

    /// 已缓存的张数（诊断 / 测试）。
    #[cfg(test)]
    pub(crate) fn cached(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane_with(wanted: &[&str]) -> MediaLane {
        let mut lane = MediaLane::default();
        lane.observe(wanted.iter().map(|id| (*id).to_string()));
        lane
    }

    #[test]
    fn only_what_is_on_screen_is_ever_requested() {
        let mut lane = lane_with(&["a", "b", "a"]);
        assert_eq!(lane.requests(), vec!["a", "b"], "同一帧里重复的只抓一次");
        assert_eq!(lane.requests(), Vec::<String>::new(), "在飞的不重复抓");

        // 滚出视口：目标集重算，屏幕上的图才有理由抓。
        lane.observe(["c".to_string()]);
        assert_eq!(lane.requests(), vec!["c"]);
    }

    #[test]
    fn a_supplied_picture_is_not_requested_again() {
        let mut lane = lane_with(&["a"]);
        assert_eq!(lane.requests(), vec!["a"]);
        lane.supply("a", Arc::new(vec![1, 2, 3]));
        assert_eq!(lane.requests(), Vec::<String>::new());
        assert_eq!(lane.bytes("a").map(|b| b.len()), Some(3));
    }

    #[test]
    fn a_failure_is_not_retried_until_the_picture_leaves_the_screen() {
        let mut lane = lane_with(&["a"]);
        assert_eq!(lane.requests(), vec!["a"]);
        lane.fail("a");
        assert!(lane.is_failed("a"));
        assert_eq!(lane.requests(), Vec::<String>::new(), "屏幕上不重试");
        // 滚出去再滚回来：重新给一次机会。
        lane.observe(std::iter::empty());
        assert!(!lane.is_failed("a"), "离开视口的失败被忘掉");
        lane.observe(["a".to_string()]);
        assert_eq!(lane.requests(), vec!["a"]);
    }

    #[test]
    fn a_content_rebuild_forgets_the_generation_but_keeps_the_bytes() {
        let mut lane = lane_with(&["a", "b"]);
        lane.requests();
        lane.supply("a", Arc::new(vec![0; 4]));
        lane.fail("b");
        lane.forget_content();
        assert!(!lane.is_failed("b"));
        assert_eq!(lane.cached(), 1, "字节是内容寻址的，重建之后依然有效");
        assert_eq!(lane.watch.len(), 0, "旧一代的屏幕目标集不能带进新内容");
        // 新一代观察到同样的图：失败的那张重新试，已缓存的那张不问。
        lane.observe(["a".to_string(), "b".to_string()]);
        assert_eq!(lane.requests(), vec!["b"]);
    }

    /// 字节预算（硬上限）被逼到要挤在屏的图时：那一张在这一代里不再要——否则「挤出 →
    /// 下一帧重抓 → 回执再把它挤回来」是一个不需要用户输入的自激环。
    #[test]
    fn a_picture_evicted_by_the_byte_budget_is_not_asked_for_again_while_on_screen() {
        let mut lane = MediaLane::default();
        lane.observe(["a".to_string(), "b".to_string()]);
        // 两张都在屏上，加起来超预算：后进的把先前的挤出缓存。
        lane.supply("a", Arc::new(vec![0; BYTE_BUDGET / 2 + 1]));
        lane.supply("b", Arc::new(vec![0; BYTE_BUDGET / 2 + 1]));
        assert_eq!(lane.cached(), 1, "预算硬上限：装不下两张");
        assert!(lane.bytes("a").is_none(), "最久未用的那张先走");
        assert!(lane.is_failed("a"), "被预算挤掉的在屏图记成「不再要」");
        assert_eq!(lane.requests(), Vec::<String>::new(), "不形成抓取环");
        // 滚出视口 → 忘掉那条记录；滚回来 → 重新试一次。
        lane.observe(["b".to_string()]);
        lane.observe(["a".to_string(), "b".to_string()]);
        assert_eq!(lane.requests(), vec!["a"]);
    }

    #[test]
    fn inflight_is_bounded() {
        let ids: Vec<String> = (0..MAX_INFLIGHT + 5).map(|i| format!("m{i}")).collect();
        let mut lane = MediaLane::default();
        lane.observe(ids.iter().cloned());
        assert_eq!(lane.requests().len(), MAX_INFLIGHT);
        assert_eq!(lane.requests(), Vec::<String>::new(), "上限到了就等回执");
        // 回执腾出位置：下一帧继续。
        lane.supply("m0", Arc::new(vec![0; 1]));
        let next = lane.requests();
        assert_eq!(next.len(), 1, "{next:?}");
        assert!(next[0].starts_with('m'));
    }

    #[test]
    fn the_byte_cache_evicts_the_least_recently_used() {
        let mut lane = MediaLane::default();
        let ids: Vec<String> = (0..BYTE_ENTRIES).map(|i| format!("m{i}")).collect();
        for id in &ids {
            lane.supply(id, Arc::new(vec![0; 8]));
        }
        assert_eq!(lane.cached(), BYTE_ENTRIES);
        // 摸一下最旧的，让它变成最新。
        lane.bytes("m0").expect("cached");
        lane.supply("fresh", Arc::new(vec![0; 8]));
        assert_eq!(lane.cached(), BYTE_ENTRIES, "张数上限生效");
        assert!(lane.bytes("m0").is_some(), "刚用过的留下来了");
        assert!(lane.bytes("m1").is_none(), "最久未用的被淘汰");
    }

    /// 同屏超过软张数目标时，在屏的图不许被自己的数量挤掉——那是「每帧重抓」的自激环。
    #[test]
    fn a_screenful_of_pictures_is_never_evicted_by_its_own_count() {
        let ids: Vec<String> = (0..BYTE_ENTRIES + 4).map(|i| format!("m{i}")).collect();
        let mut lane = MediaLane::default();
        lane.observe(ids.iter().cloned());
        for id in &ids {
            lane.supply(id, Arc::new(vec![0; 8]));
        }
        assert_eq!(lane.cached(), ids.len(), "一屏的图全部留下");
        assert_eq!(
            lane.requests(),
            Vec::<String>::new(),
            "不重抓：屏幕上刚抓到的那些还在缓存里"
        );
    }

    /// 淘汰先走离屏的：滚回去再滚回来的图不该把在屏的图挤掉。
    #[test]
    fn eviction_prefers_the_pictures_that_left_the_screen() {
        let mut lane = lane_with(&["on-screen-a", "on-screen-b"]);
        lane.supply("on-screen-a", Arc::new(vec![0; 8]));
        lane.supply("on-screen-b", Arc::new(vec![0; 8]));
        // 一堆离屏的图把它们挤走：张数目标 ≥ 8，所以淘汰掉的是离屏的那几张。
        for i in 0..BYTE_ENTRIES {
            lane.supply(&format!("gone-{i}"), Arc::new(vec![0; 8]));
        }
        assert!(lane.bytes("on-screen-a").is_some(), "在屏的不被挤掉");
        assert!(lane.bytes("on-screen-b").is_some());
        assert!(lane.bytes("gone-0").is_none(), "最久未用的离屏图先走");
    }

    #[test]
    fn one_oversized_picture_stays_alone_in_the_cache() {
        let mut lane = MediaLane::default();
        lane.supply("big", Arc::new(vec![0; BYTE_BUDGET + 1]));
        lane.supply("small", Arc::new(vec![0; 8]));
        assert_eq!(lane.cached(), 1);
        assert!(
            lane.bytes("small").is_some(),
            "超预算的那张被挤出去（它自己进来时至少留自己）"
        );
    }

    #[test]
    fn a_picture_whose_bytes_were_evicted_is_requested_again() {
        let mut lane = lane_with(&["a"]);
        lane.supply("a", Arc::new(vec![0; 4]));
        // a 滚出视口：它的字节可以被挤掉（离屏优先）。
        lane.observe(std::iter::empty());
        for i in 0..BYTE_ENTRIES {
            lane.supply(&format!("x{i}"), Arc::new(vec![0; 4]));
        }
        assert!(lane.bytes("a").is_none(), "离屏的字节被挤出缓存");
        // 滚回来 → 没有字节 → 重新抓一次（内容寻址：抓到的是同一份）。
        lane.observe(["a".to_string()]);
        assert_eq!(lane.requests(), vec!["a"]);
    }
}
