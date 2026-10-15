//! 会话血缘森林 —— 扁平会话行 → 树序 + 连接前缀。
//!
//! 输入是一次列表投影（id / 父 id / pin / 最后交互），输出是**全序行**：
//! 树序遍历的顺序、每行所在深度、以及可直接拼进首列的渲染前缀。两个消费面
//! 共用这一份实现（`wing ps --tree` 与 TUI 的会话面板）；外部的 web / 桌面
//! 前端按同一份契约在 TS 侧另实现（见 docs/dev/session-lineage.md）。
//!
//! 排序契约（产品语义的参照点）：
//!
//! - **pin 分两层**：根的 pin（parent 不在输入集合里 = 展示为根的节点）让它
//!   上浮到跨树最前，**子树跟随**；子节点的 pin 只在它所在的兄弟组内排前——
//!   不提升自己、也不影响父节点（视图区分：置顶区只对根开放）；
//! - 兄弟组内排序：pin 的在前（pin 时间降序、无时间的次之），其余按
//!   **子树最后交互时间**降序（最近有动静的任务树浮上来），并列按 id 升序；
//! - parent 不在集合（被过滤 / 幽灵引用）→ 该节点升为根；
//! - 环（只可能来自手改 metadata）→ 环上节点全部升为根（诚实降级，不挂死）。
//!
//! 前缀用 `├─` / `└─` / `│` 三字形，根不带前缀（树与树之间不连线，每棵树
//! 独立成画）；缩进层数封顶 [`MAX_INDENT_DEPTH`]（人为深链防御，超出的祖先
//! 层不再加宽）。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::shared::pinning::parse_timestamp_micros;

/// 渲染前缀的最大缩进层数（更深的部分不再加宽，防止深链把整行推爆）。
pub const MAX_INDENT_DEPTH: usize = 16;

/// 一行输入：列表投影里与「树」有关的最小切片。
#[derive(Debug, Clone, Copy)]
pub struct TreeRow<'a> {
    pub id: &'a str,
    /// 创建者会话 id（`parent_session_id`）；根为 `None`。
    pub parent: Option<&'a str>,
    /// 是否被 pin（`pin` 标签；前端约定，见 [`crate::shared::pinning`]）。
    pub pinned: bool,
    /// pin 时间（`tag_meta.pin.added_at`）。
    pub pin_added_at: Option<&'a str>,
    /// 最后一次交互（本地 naive ISO，与后端 `last_interaction` 同口径）。
    pub last_interaction: Option<&'a str>,
}

/// 树里被放置的一行（输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// 输入行的下标——消费方据此取回自己的整行。
    pub index: usize,
    /// 树深（0 = 根）。
    pub depth: usize,
    /// 渲染前缀（缩进 + 连接符；根为 `""`）。
    pub prefix: String,
}

/// 把会话行折成森林并排出展示全序。
///
/// 输入顺序不参与定序（每一项的排序键都是自足的：pin 时间 / 子树活跃 /
/// id），因此同一批会话在任何消费面都会得到同一个顺序。
pub fn build_forest(rows: &[TreeRow<'_>]) -> Vec<TreeEntry> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }

    // id → 下标。重 id 取先到者：id 是会话的唯一键，重复只会来自构造错误的
    // 输入，这里保证不 panic、不挂死即可（与幽灵父同级的降级）。
    let mut index_of: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (i, row) in rows.iter().enumerate() {
        index_of.entry(row.id).or_insert(i);
    }

    // 直父（在集合内）→ **有效父**：整条链无环才算数。环上节点全部断开
    // （升根）——这不是「怎么渲染坏数据」的问题，是「怎么不挂死」的问题。
    let direct: Vec<Option<usize>> = rows
        .iter()
        .map(|row| row.parent.and_then(|p| index_of.get(p).copied()))
        .collect();
    let parent: Vec<Option<usize>> = (0..n)
        .map(|i| match direct[i] {
            Some(p) if chain_is_acyclic(i, p, &direct) => Some(p),
            _ => None,
        })
        .collect();

    // 深度（环已断开，向上走必达根）。
    let depth_of = |mut i: usize| -> usize {
        let mut depth = 0;
        while let Some(p) = parent[i] {
            depth += 1;
            i = p;
        }
        depth
    };

    // 子树「最后交互」分数 = 自身与全部后代的最大值：深到浅上浮，一次算全。
    // 无时间 / 不可解析 = None，排序时垫底（Option 的序天然如此）。
    let mut score: Vec<Option<i64>> = rows
        .iter()
        .map(|row| row.last_interaction.and_then(parse_timestamp_micros))
        .collect();
    let mut by_depth: Vec<usize> = (0..n).collect();
    by_depth.sort_by_key(|&i| std::cmp::Reverse(depth_of(i)));
    for &i in &by_depth {
        if let Some(p) = parent[i]
            && score[i] > score[p]
        {
            score[p] = score[i];
        }
    }

    // children + roots。
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, slot) in parent.iter().enumerate() {
        match slot {
            Some(p) => children[*p].push(i),
            None => roots.push(i),
        }
    }

    // 排序键：pin 在前（时间降序）→ 子树活跃降序 → id 升序（并列定序，也
    // 保证跨消费面一致）。pin 时间只对 pin 的节点有意义：其余取 None。
    let pin_micros: Vec<Option<i64>> = rows
        .iter()
        .map(|row| {
            if row.pinned {
                row.pin_added_at.and_then(parse_timestamp_micros)
            } else {
                None
            }
        })
        .collect();
    let cmp = |a: usize, b: usize| -> Ordering {
        rows[b]
            .pinned
            .cmp(&rows[a].pinned)
            .then_with(|| pin_micros[b].cmp(&pin_micros[a]))
            .then_with(|| score[b].cmp(&score[a]))
            .then_with(|| rows[a].id.cmp(rows[b].id))
    };
    roots.sort_by(|&a, &b| cmp(a, b));
    for group in children.iter_mut() {
        group.sort_by(|&a, &b| cmp(a, b));
    }

    // DFS 放置；guide 是「祖先层还有没有后续兄弟」的竖线栈（每层画 `│` 与否）。
    let mut out: Vec<TreeEntry> = Vec::with_capacity(n);
    let mut guide: Vec<bool> = Vec::new();
    for &root in &roots {
        walk(root, 0, true, &mut guide, &children, &mut out);
    }
    out
}

/// `i` 的直父 `p`：从 `p` 出发能否走到根而不回到 `i`（也不在链上重访）。
///
/// 集合有限 ⇒「不重访」的运行必然终止；重访即环。
fn chain_is_acyclic(i: usize, p: usize, direct: &[Option<usize>]) -> bool {
    let mut seen: HashSet<usize> = HashSet::from([i]);
    let mut cur = p;
    loop {
        if !seen.insert(cur) {
            return false;
        }
        match direct[cur] {
            Some(next) => cur = next,
            None => return true,
        }
    }
}

/// 放置 `idx` 并递归其子树。
///
/// `guide` 的长度 = 当前节点上方各祖先层的连线槽；`is_last` 是「在本兄弟组
/// 里是否最后一个」（只对非根有意义）。根不产生连线槽——每棵树独立成画，
/// 根与根之间不连线。
fn walk(
    idx: usize,
    depth: usize,
    is_last: bool,
    guide: &mut Vec<bool>,
    children: &[Vec<usize>],
    out: &mut Vec<TreeEntry>,
) {
    let prefix = if depth == 0 {
        String::new()
    } else {
        let start = guide.len().saturating_sub(MAX_INDENT_DEPTH);
        let mut prefix = String::new();
        for &more in &guide[start..] {
            prefix.push_str(if more { "│  " } else { "   " });
        }
        prefix.push_str(if is_last { "└─ " } else { "├─ " });
        prefix
    };
    out.push(TreeEntry {
        index: idx,
        depth,
        prefix,
    });

    if depth > 0 {
        // 本节点还有后续兄弟 → 孩子那一层的竖线继续。
        guide.push(!is_last);
    }
    let kids = &children[idx];
    for (k, &child) in kids.iter().enumerate() {
        walk(child, depth + 1, k + 1 == kids.len(), guide, children, out);
    }
    if depth > 0 {
        guide.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(id: &'a str, parent: Option<&'a str>) -> TreeRow<'a> {
        TreeRow {
            id,
            parent,
            pinned: false,
            pin_added_at: None,
            last_interaction: None,
        }
    }

    fn pinned<'a>(mut r: TreeRow<'a>, at: Option<&'a str>) -> TreeRow<'a> {
        r.pinned = true;
        r.pin_added_at = at;
        r
    }

    fn active<'a>(mut r: TreeRow<'a>, at: &'a str) -> TreeRow<'a> {
        r.last_interaction = Some(at);
        r
    }

    /// 输出的 (id, depth, prefix) 三元组（用输入下标取回 id）。
    fn shape<'a>(rows: &[TreeRow<'a>], out: &[TreeEntry]) -> Vec<(&'a str, usize, String)> {
        out.iter()
            .map(|e| (rows[e.index].id, e.depth, e.prefix.clone()))
            .collect()
    }

    #[test]
    fn empty_input_places_nothing() {
        assert!(build_forest(&[]).is_empty());
    }

    #[test]
    fn single_root_has_no_prefix() {
        let rows = [row("a", None)];
        let out = build_forest(&rows);
        assert_eq!(shape(&rows, &out), [("a", 0, String::new())]);
    }

    #[test]
    fn children_follow_the_parent_with_drawing_prefixes() {
        let rows = [row("A", None), row("c1", Some("A")), row("c2", Some("A"))];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [
                ("A", 0, String::new()),
                ("c1", 1, "├─ ".to_string()),
                ("c2", 1, "└─ ".to_string()),
            ]
        );
    }

    #[test]
    fn grandchild_keeps_the_trunk_line_of_a_non_last_ancestor() {
        // A → B（B 之后还有兄弟 C）→ G。G 的祖先前缀里 B 层要画 `│`。
        let rows = [
            row("A", None),
            row("B", Some("A")),
            row("C", Some("A")),
            row("G", Some("B")),
        ];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [
                ("A", 0, String::new()),
                ("B", 1, "├─ ".to_string()),
                ("G", 2, "│  └─ ".to_string()),
                ("C", 1, "└─ ".to_string()),
            ]
        );
    }

    #[test]
    fn pinned_root_floats_to_the_top_with_its_subtree() {
        let rows = [
            row("r1", None),
            pinned(row("r2", None), Some("2026-10-01T09:00:00")),
            row("r2child", Some("r2")),
        ];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [
                ("r2", 0, String::new()),
                ("r2child", 1, "└─ ".to_string()),
                ("r1", 0, String::new()),
            ]
        );
    }

    #[test]
    fn pinned_child_orders_first_among_siblings_without_moving_its_parent() {
        // P 的活跃时间是旧的（排在 r2 之后）；子 c2 被 pin——c2 在 P 的子组
        // 里排第一，但 P 在根组的位置不动（pin 时间不算进父的排序键）。
        let rows = [
            active(row("P", None), "2026-10-01T00:00:00"),
            active(row("r2", None), "2026-10-02T00:00:00"),
            pinned(row("c2", Some("P")), Some("2026-10-03T00:00:00")),
            row("c1", Some("P")),
        ];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [
                ("r2", 0, String::new()),
                ("P", 0, String::new()),
                ("c2", 1, "├─ ".to_string()),
                ("c1", 1, "└─ ".to_string()),
            ]
        );
    }

    #[test]
    fn newer_pin_time_wins_between_pinned_siblings() {
        let rows = [
            row("P", None),
            pinned(row("old", Some("P")), Some("2026-10-01T09:00:00")),
            pinned(row("new", Some("P")), Some("2026-10-06T09:00:00")),
        ];
        let out = build_forest(&rows);
        let order: Vec<&str> = shape(&rows, &out).into_iter().map(|(id, ..)| id).collect();
        assert_eq!(order, ["P", "new", "old"]);
    }

    #[test]
    fn pinned_without_time_sorts_after_timed_pins() {
        let rows = [
            pinned(row("timed", None), Some("2026-10-05T09:00:00")),
            pinned(row("unknown", None), None),
            pinned(row("garbage", None), Some("not-a-time")),
        ];
        let out = build_forest(&rows);
        let order: Vec<&str> = shape(&rows, &out).into_iter().map(|(id, ..)| id).collect();
        // 有时间的在前；「未知」与「不可解析」同等垫底，彼此按 id 升序
        // （确定性 tie-break：garbage < unknown）。
        assert_eq!(order, ["timed", "garbage", "unknown"]);
    }

    #[test]
    fn subtree_activity_orders_roots() {
        // P 自己很旧，但它的子树里有新活动 → 整棵树上浮到 Q 之前。
        let rows = [
            active(row("P", None), "2026-10-01T00:00:00"),
            active(row("Q", None), "2026-10-02T00:00:00"),
            active(row("X", Some("P")), "2026-10-03T00:00:00"),
        ];
        let out = build_forest(&rows);
        let order: Vec<&str> = shape(&rows, &out).into_iter().map(|(id, ..)| id).collect();
        assert_eq!(order, ["P", "X", "Q"]);
    }

    #[test]
    fn missing_times_sort_last_and_ties_break_on_id() {
        let rows = [
            row("b", None),
            active(row("a", None), "2026-10-01T00:00:00"),
            row("c", None),
        ];
        let out = build_forest(&rows);
        let order: Vec<&str> = shape(&rows, &out).into_iter().map(|(id, ..)| id).collect();
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn orphan_parent_is_a_root() {
        let rows = [
            row("x", Some("ghost")), // parent 不在集合里
            row("y", None),
        ];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [("x", 0, String::new()), ("y", 0, String::new()),]
        );
    }

    #[test]
    fn cycles_collapse_into_roots_without_hanging() {
        let rows = [row("a", Some("b")), row("b", Some("a"))];
        let out = build_forest(&rows);
        assert_eq!(
            shape(&rows, &out),
            [("a", 0, String::new()), ("b", 0, String::new()),]
        );
    }

    #[test]
    fn a_cycle_on_one_branch_does_not_poison_healthy_rows() {
        // P 是正常根，其子 c 与 z 互相成环 → c/z 升根，P 与 s 保持完好。
        let rows = [
            row("P", None),
            row("s", Some("P")),
            row("c", Some("z")),
            row("z", Some("c")),
        ];
        let out = build_forest(&rows);
        let (dtree, _): (Vec<_>, Vec<_>) = shape(&rows, &out)
            .into_iter()
            .partition(|(id, ..)| ["P", "s"].contains(id));
        assert_eq!(
            dtree,
            [("P", 0, String::new()), ("s", 1, "└─ ".to_string()),]
        );
    }

    #[test]
    fn deep_chains_stop_widening_at_the_cap() {
        // 20 层链：最深处的 prefix 宽度被 MAX_INDENT_DEPTH 封顶。
        let ids: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        let rows: Vec<TreeRow<'_>> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let parent = if i == 0 {
                    None
                } else {
                    Some(ids[i - 1].as_str())
                };
                row(id, parent)
            })
            .collect();
        let out = build_forest(&rows);
        let deepest = out.last().expect("chain placed");
        assert_eq!(deepest.depth, 19);
        // 每层最多 3 列（`│  ` / `   `）+ 2 列连接符（`└─` + 空格）→ 封顶宽度。
        assert!(
            deepest.prefix.chars().count() <= MAX_INDENT_DEPTH * 3 + 3,
            "prefix not capped: {:?}",
            deepest.prefix
        );
        assert!(deepest.prefix.ends_with("└─ "));
    }
}
