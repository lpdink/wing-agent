//! 会话血缘森林 —— 扁平会话行 → 树序 + 连接前缀 + crew 聚合 + 层投影。
//!
//! 输入是一次列表投影（id / 父 id / pin / 最后交互 / 行自身状态），输出是一份
//! [`Forest`]：展示全序（树序遍历 + 每行的深度与渲染前缀）、每个节点的后代聚合
//! （[`Crew`]）、以及「给定 scope 取一层」的投影（[`Forest::level`]）。三个消费面
//! 共用这一份实现（TUI 的会话面板、`wing ps`（`--tree` 与层视图）、`wing usage
//! --tree`）；外部的 web / 桌面前端按同一份契约在 TS 侧另实现
//! （见 docs/dev/session-lineage.md）。
//!
//! 排序契约（产品语义的参照点）：
//!
//! - **pin 分两层**：根的 pin（parent 不在输入集合里 = 展示为根的节点）让它
//!   上浮到跨树最前，**子树跟随**；子节点的 pin 只在它所在的兄弟组内排前——
//!   不提升自己、也不影响父节点（视图区分：置顶区只对根开放）；
//! - 兄弟组内排序：pin 的在前（pin 时间降序、无时间的次之）→ **子树注意力**
//!   （自己或任一后代在等人类回答的在前）→ **子树最后交互时间**降序（最近有
//!   动静的任务树浮上来）→ id 升序（并列定序，跨消费面一致）→ 输入下标兜底
//!   （同 id 的重复行只可能来自构造错误的输入）；
//! - 根层与任意子层用同一条比较器。pin 的两层语义来自「置顶区只对根开放」的
//!   视图事实，注意力则是**子树属性**（有后代在等你 → 整棵树上浮），两者刻意
//!   不对称；
//! - parent 不在集合（被过滤 / 幽灵引用）→ 该节点升为根；
//! - 环（只可能来自手改 metadata）→ 环上节点全部升为根（诚实降级，不挂死）。
//!
//! **crew 聚合**（[`Crew`]）：每个节点带后代总数（含 dormant——它回答「这个任务
//! 有多大」，不是「这一屏有多少行」）、后代里在等人类回答 / 在跑轮次的条数、
//! 子树 `max(last_interaction)`。一次自底向上算全，与排序用的子树活跃同源。
//!
//! **存活闭包与层投影**：`kept(r) = live(r) ∨ pinned(r) ∨ ∃ 子 c: kept(c)`，其中
//! `live = status != inactive`（状态未知按存活）。闭包是必要的：否则一个 dormant
//! 的父任务会凭空消失，「层 = 任务」退化成「一堆散点 crew 升格成根」。
//! [`Forest::level`] 取某 scope 的**直接子** ∩ 保留集（`include_dormant` 时不收），
//! 顺带给出本层被收起的直接子数。
//!
//! 不变量（本模块的测试锚定）：投影是 `(rows, scope, include_dormant)` 的**纯函数**
//! ——不看时钟、不看随机数、不看输入顺序（同 id 的重复行例外：只可能来自畸形的输入，
//! 届时按输入序兜底成全序）；投影**不改变任何一行的相对顺序**；保留集里每个节点在它
//! 所属的那一层**恰好出现一次**（无重复、无孤儿）。
//!
//! **下标空间是接口契约**：投影吐出的 `usize` 是 [`Forest::build`] 收到的那个切片的
//! 坐标，消费面必须按原序留着它（详见 [`Forest`] 的文档）。
//!
//! 前缀用 `├─` / `└─` / `│` 三字形，根不带前缀（树与树之间不连线，每棵树
//! 独立成画）；缩进层数封顶 [`MAX_INDENT_DEPTH`]（人为深链防御，超出的祖先
//! 层不再加宽）。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::protocol::SessionStatus;
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
    /// 行自身状态（列表行的 `status`，用 [`SessionStatus::parse`] 填）。它只被问
    /// 三件事：`waiting` = 注意力、`working` = crew 计数、`inactive` = 存活。
    /// `None` = 这个消费面没有状态可给（`wing usage --tree`）：按存活处理、不参与
    /// 注意力、不计 working。
    pub status: Option<SessionStatus>,
}

/// 树里被放置的一行（输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// 输入行的下标——消费方据此取回自己的整行。下标空间是传给 [`Forest::build`]
    /// 的那个 `rows` 切片，所以消费面必须按原序留着它，别在两次投影之间重排。
    pub index: usize,
    /// 树深（0 = 根）。
    pub depth: usize,
    /// 渲染前缀（缩进 + 连接符；根为 `""`）。
    pub prefix: String,
}

/// 一个节点的 crew 聚合。三个计数只数**后代**（含 dormant 后代）；`activity` 是
/// **含自己**的子树 max。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crew {
    /// 后代总数。
    pub descendants: usize,
    /// 后代里在等人类回答（`waiting`）的条数。注意它不含自己——「本行有没有注意力」
    /// 还要看自己的状态（见模块头的注意力定义）。
    pub waiting: usize,
    /// 后代里在跑轮次（`working`）的条数。
    pub working: usize,
    /// 子树 `max(last_interaction)`（微秒；无时间 / 不可解析 = `None`）。与排序用的
    /// 子树活跃是同一个值，**只服务排序**：要显示请用 [`Forest::activity_of`] 取来源行的
    /// 原文，别把微秒格式化回来。
    pub activity: Option<i64>,
}

/// 层投影的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Level {
    /// 本层要显示的行：输入下标，顺序 = 森林全序在该层的限制。
    pub rows: Vec<usize>,
    /// 本层被收起的直接子数（`include_dormant` 时恒为 0——一行都没收起）。
    pub dormant_children: usize,
}

/// 一次构建的全部派生事实：拓扑（有效父 / 子 / 深度）、crew 聚合、存活闭包、
/// 展示全序。五个投影（[`Self::tree`] / [`Self::crew`] / [`Self::activity_of`] /
/// [`Self::parent_of`] / [`Self::level`]）都是它的只读视图——消费面构建一次，之后按需取。
///
/// **下标空间是接口契约**：投影吐出的 `usize` 永远是调用方传给 [`Self::build`] 的
/// 那个切片的坐标。消费面按原序持有自己的行（不要在两次投影之间 sort / filter 那份
/// 数组），否则投影会静默指到别的行——不 panic、不报错，只是画错。
///
/// **输入是未按存活过滤的行集**：存活闭包、`dormant_children` 与 [`Crew`] 的三个计数都按
/// **全集**算——消费面若在 [`Self::build`] 之前按存活剪过行，三件事一起失效（一个
/// dormant 父被剪掉，它的活子就升成假根，crew 也少算那些后代）。过滤是投影的事，
/// 不能发生在投影之前。
#[derive(Debug, Default)]
pub struct Forest {
    /// id → 输入下标（重 id 取先到者）。
    index_of: HashMap<String, usize>,
    /// 有效父（幽灵父 / 环已断开）。
    parent: Vec<Option<usize>>,
    /// 每个节点的直接子，已按排序契约排好（层投影只过滤、不重排）。
    children: Vec<Vec<usize>>,
    /// 根，已按排序契约排好。
    roots: Vec<usize>,
    /// 存活闭包（`include_dormant` 之前的那一版）。
    kept: Vec<bool>,
    crew: Vec<Crew>,
    /// 子树活跃来自哪一行（输入下标）：`crew` 的 `activity` 与它同源。
    activity_row: Vec<usize>,
    tree: Vec<TreeEntry>,
}

impl Forest {
    /// 把会话行折成森林：拓扑 + 聚合 + 闭包 + 全序，一次算全。
    ///
    /// 每一项的排序键都是自足的（pin 时间 / 注意力 / 子树活跃 / id），因此同一批会话
    /// 在任何消费面都会得到同一个顺序。唯一例外是同 id 的重复行（只可能来自构造错误
    /// 的输入）：四把键全并列时按输入下标兜底成全序。
    pub fn build(rows: &[TreeRow<'_>]) -> Self {
        let n = rows.len();
        if n == 0 {
            return Self::default();
        }

        // id → 下标。重 id 取先到者：id 是会话的唯一键，重复只会来自构造错误的
        // 输入，这里保证不 panic、不挂死即可（与幽灵父同级的降级）。
        let mut index_of: HashMap<String, usize> = HashMap::with_capacity(n);
        for (i, row) in rows.iter().enumerate() {
            index_of.entry(row.id.to_string()).or_insert(i);
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
        let depth: Vec<usize> = (0..n).map(depth_of).collect();

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut roots: Vec<usize> = Vec::new();
        for (i, slot) in parent.iter().enumerate() {
            match slot {
                Some(p) => children[*p].push(i),
                None => roots.push(i),
            }
        }

        // 自底向上（深 → 浅，父的深度严格小于子）一遍算全：crew 聚合 + 存活闭包。
        let mut crew: Vec<Crew> = rows
            .iter()
            .map(|row| Crew {
                activity: row.last_interaction.and_then(parse_timestamp_micros),
                ..Crew::default()
            })
            .collect();
        // 子树活跃的来源行：默认自己（整棵子树都解析不了时消费面据此显示自己的原文，
        // 而不是把解析失败画成空白），有更大的孩子就跟着孩子走。
        let mut activity_row: Vec<usize> = (0..n).collect();
        let mut kept: Vec<bool> = rows
            .iter()
            .map(|row| is_live(row.status) || row.pinned)
            .collect();
        let mut by_depth: Vec<usize> = (0..n).collect();
        by_depth.sort_by_key(|&i| std::cmp::Reverse(depth[i]));
        for &i in &by_depth {
            let Some(p) = parent[i] else { continue };
            crew[p].descendants += crew[i].descendants + 1;
            crew[p].waiting +=
                crew[i].waiting + usize::from(rows[i].status == Some(SessionStatus::Waiting));
            crew[p].working +=
                crew[i].working + usize::from(rows[i].status == Some(SessionStatus::Working));
            if crew[i].activity > crew[p].activity {
                crew[p].activity = crew[i].activity;
                activity_row[p] = activity_row[i];
            }
            kept[p] = kept[p] || kept[i];
        }

        // 子树注意力：自己或任一后代在等人类回答。
        let attention: Vec<bool> = (0..n)
            .map(|i| rows[i].status == Some(SessionStatus::Waiting) || crew[i].waiting > 0)
            .collect();
        // pin 时间只对 pin 的节点有意义：其余取 None。
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
        // pin 在前（时间降序）→ 子树注意力 → 子树活跃降序 → id 升序 → 输入下标
        // （同 id 的重复行按输入序定，不依赖排序算法的稳定性）。
        let cmp = |a: usize, b: usize| -> Ordering {
            rows[b]
                .pinned
                .cmp(&rows[a].pinned)
                .then_with(|| pin_micros[b].cmp(&pin_micros[a]))
                .then_with(|| attention[b].cmp(&attention[a]))
                .then_with(|| crew[b].activity.cmp(&crew[a].activity))
                .then_with(|| rows[a].id.cmp(rows[b].id))
                .then_with(|| a.cmp(&b))
        };
        roots.sort_by(|&a, &b| cmp(a, b));
        for group in children.iter_mut() {
            group.sort_by(|&a, &b| cmp(a, b));
        }

        // DFS 放置；guide 是「祖先层还有没有后续兄弟」的竖线栈（每层画 `│` 与否）。
        let mut tree: Vec<TreeEntry> = Vec::with_capacity(n);
        let mut guide: Vec<bool> = Vec::new();
        for &root in &roots {
            walk(root, 0, true, &mut guide, &children, &mut tree);
        }

        Self {
            index_of,
            parent,
            children,
            roots,
            kept,
            crew,
            activity_row,
            tree,
        }
    }

    /// 展示全序：树序遍历的每一行（输入下标 + 深度 + 渲染前缀）。`--tree` 与面板的
    /// 全量平铺都直接画它。**内核不删任何一行**：画出来的是它**收到的**拓扑，消费面
    /// 若在 [`Self::build`] 之前剪过行，画的就是剪过的拓扑（前提见 [`Forest`]）；
    /// 收起到某一层只发生在 [`Self::level`]。
    pub fn tree(&self) -> &[TreeEntry] {
        &self.tree
    }

    /// 该行（[`Self::tree`] / [`Self::level`] 给出的输入下标）的 crew 聚合；越界 = 空
    /// 聚合（调用方只该传自己拿到的行下标）。
    pub fn crew(&self, index: usize) -> Crew {
        self.crew.get(index).copied().unwrap_or_default()
    }

    /// 子树活跃**来自哪一行**（输入下标，与投影同一份下标空间）：`crew(index).activity`
    /// 就是从这一行的 `last_interaction` 解析出来的。
    ///
    /// 消费面据此显示子树活跃的**原文**——自己格式化回来会与各自的既有口径分叉（面板是
    /// `MM-DD HH:MM`、CLI 是年龄词表），而同一时刻的两种书写也不能靠微秒反查（那是有损键）。
    /// 整棵子树都没有可解析的时间时回落到该行自己（此时 `activity` 为 `None`）：消费面
    /// 仍能显示自己的原文，而不是把解析失败画成空白。越界 = `None`（同 [`Self::crew`]）。
    pub fn activity_of(&self, index: usize) -> Option<usize> {
        self.activity_row.get(index).copied()
    }

    /// 有效父（输入下标）：根 / 幽灵父 / 环 = `None`。面包屑按它向上走，不要拿原始
    /// `parent_session_id` 走——那条链可能带环。
    pub fn parent_of(&self, index: usize) -> Option<usize> {
        self.parent.get(index).copied().flatten()
    }

    /// 层投影：`scope` 的直接子 ∩ 保留集，顺序 = 森林全序在该层的限制。
    ///
    /// `None` = 根层；`Some(id)` = 该会话的子层，未知 id（会话已被逐出 / 归档）→ 空层，
    /// 诚实降级不猜。重复 id 只解析到先到者（后到者的子层取不到，与 `index_of` 同一条
    /// 降级）。`include_dormant` 由消费面决定：要显示 dormant 行时置 true（一律保留，
    /// `dormant_children` 归零）。
    ///
    /// 输入前提见 [`Forest`] 的文档（未按存活过滤的行集）；否则保留集 ≡ 输入集，
    /// 闭包与 `dormant_children` 一起退化成 no-op。
    pub fn level(&self, scope: Option<&str>, include_dormant: bool) -> Level {
        let children: &[usize] = match scope {
            None => &self.roots,
            Some(id) => match self.index_of.get(id) {
                Some(&index) => &self.children[index],
                None => return Level::default(),
            },
        };
        let mut level = Level::default();
        for &index in children {
            if include_dormant || self.kept[index] {
                level.rows.push(index);
            } else {
                level.dormant_children += 1;
            }
        }
        level
    }
}

/// 把会话行折成森林并排出展示全序（只要树、不要聚合 / 层投影的消费面用这个）。
pub fn build_forest(rows: &[TreeRow<'_>]) -> Vec<TreeEntry> {
    Forest::build(rows).tree().to_vec()
}

/// 存活：不在内存（`inactive`）才算 dormant。状态未知（`None`）按存活处理——
/// 「不知道」不能当成「已逐出」把行藏起来。（面板另有一条：不认识的状态串按 `inactive`
/// 画图标 —— 那是「画成什么样」，不是「留不留这一行」，两个问题各管各的。）
///
/// 它是保留集的**种子**（`build` 里再并上 pin 与「有被保留的子」）。`wing ps` 的
/// `--flat` / `--tree` / `--json` 走的是早先的行集（逐字兼容）：默认（无 `--all` /
/// `--tag` / `--parent`）先丢掉 `inactive`，`--tree` 因此是在剪过的集合上建森林——闭包
/// 不生效，被逐出的父不成行、它的活子升成根；`--flat` / `--json` 干脆不建森林。默认的
/// 层视图不走那条路：它在全集上建森林，过滤发生在投影之后。
fn is_live(status: Option<SessionStatus>) -> bool {
    status != Some(SessionStatus::Inactive)
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
            status: None,
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

    fn with<'a>(mut r: TreeRow<'a>, status: SessionStatus) -> TreeRow<'a> {
        r.status = Some(status);
        r
    }

    fn waiting(r: TreeRow<'_>) -> TreeRow<'_> {
        with(r, SessionStatus::Waiting)
    }

    fn working(r: TreeRow<'_>) -> TreeRow<'_> {
        with(r, SessionStatus::Working)
    }

    fn inactive(r: TreeRow<'_>) -> TreeRow<'_> {
        with(r, SessionStatus::Inactive)
    }

    /// 输出的 (id, depth, prefix) 三元组（用输入下标取回 id）。
    fn shape<'a>(rows: &[TreeRow<'a>], out: &[TreeEntry]) -> Vec<(&'a str, usize, String)> {
        out.iter()
            .map(|e| (rows[e.index].id, e.depth, e.prefix.clone()))
            .collect()
    }

    /// 树的全序（只看 id）。
    fn order<'a>(rows: &[TreeRow<'a>], entries: &[TreeEntry]) -> Vec<&'a str> {
        entries.iter().map(|e| rows[e.index].id).collect()
    }

    /// 一层的行（只看 id，顺序 = 投影给出的顺序）。
    fn ids<'a>(rows: &[TreeRow<'a>], level: &Level) -> Vec<&'a str> {
        level.rows.iter().map(|&i| rows[i].id).collect()
    }

    fn micros(at: &str) -> i64 {
        parse_timestamp_micros(at).expect("fixture timestamp parses")
    }

    /// 从根层起递归收集各层的行（层序拼接 = 全序在保留集上的限制）。
    fn collect_levels<'a>(rows: &[TreeRow<'a>], forest: &Forest) -> Vec<&'a str> {
        fn walk<'a>(
            rows: &[TreeRow<'a>],
            forest: &Forest,
            scope: Option<&str>,
            out: &mut Vec<&'a str>,
        ) {
            let level = forest.level(scope, false);
            for &index in &level.rows {
                out.push(rows[index].id);
                walk(rows, forest, Some(rows[index].id), out);
            }
        }
        let mut out = Vec::new();
        walk(rows, forest, None, &mut out);
        out
    }

    #[test]
    fn empty_input_places_nothing() {
        assert!(build_forest(&[]).is_empty());
        let forest = Forest::build(&[]);
        assert!(forest.tree().is_empty());
        assert_eq!(forest.level(None, false), Level::default());
        assert_eq!(forest.level(Some("anything"), true), Level::default());
        assert_eq!(forest.parent_of(0), None);
        assert_eq!(forest.crew(0), Crew::default());
    }

    #[test]
    fn build_forest_is_the_tree_projection_of_the_forest() {
        let rows = [
            row("A", None),
            row("c", Some("A")),
            pinned(row("z", None), Some("2026-10-01T09:00:00")),
        ];
        assert_eq!(build_forest(&rows), Forest::build(&rows).tree().to_vec());
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
        assert_eq!(order(&rows, &out), ["P", "new", "old"]);
    }

    #[test]
    fn pinned_without_time_sorts_after_timed_pins() {
        let rows = [
            pinned(row("timed", None), Some("2026-10-05T09:00:00")),
            pinned(row("unknown", None), None),
            pinned(row("garbage", None), Some("not-a-time")),
        ];
        let out = build_forest(&rows);
        // 有时间的在前；「未知」与「不可解析」同等垫底，彼此按 id 升序
        // （确定性 tie-break：garbage < unknown）。
        assert_eq!(order(&rows, &out), ["timed", "garbage", "unknown"]);
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
        assert_eq!(order(&rows, &out), ["P", "X", "Q"]);
    }

    #[test]
    fn missing_times_sort_last_and_ties_break_on_id() {
        let rows = [
            row("b", None),
            active(row("a", None), "2026-10-01T00:00:00"),
            row("c", None),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["a", "b", "c"]);
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

    // ── 排序：子树注意力 ──────────────────────────────────────────

    #[test]
    fn attention_lifts_a_quiet_root_with_a_waiting_grandchild() {
        // Q 最近更活跃，但 P 的子树里有人等你 —— 祖先照样上浮（注意力是子树
        // 属性，与 pin 的「只在兄弟组内说话」刻意不同）。
        let rows = [
            active(row("P", None), "2026-10-01T00:00:00"),
            active(row("Q", None), "2026-10-02T00:00:00"),
            row("X", Some("P")),
            waiting(active(row("g", Some("X")), "2026-09-30T00:00:00")),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["P", "X", "g", "Q"]);
    }

    #[test]
    fn a_row_waiting_itself_has_attention() {
        let rows = [
            active(row("busy", None), "2026-10-05T00:00:00"),
            active(waiting(row("me", None)), "2026-10-01T00:00:00"),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["me", "busy"]);
    }

    #[test]
    fn attention_never_outranks_pin() {
        let rows = [
            pinned(
                active(row("pinned", None), "2026-09-01T00:00:00"),
                Some("2026-09-01T00:00:00"),
            ),
            active(row("attended", None), "2026-10-05T00:00:00"),
            waiting(row("kid", Some("attended"))),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["pinned", "attended", "kid"]);
    }

    #[test]
    fn a_waiting_sibling_leads_its_level_only() {
        // 同一层里：注意力压过活跃度（c-waiting 更旧仍在前）。根层的 A 也因
        // 子树注意力而领先，但 B 与 A 的兄弟关系不变。
        let rows = [
            active(row("A", None), "2026-10-03T00:00:00"),
            active(row("B", None), "2026-10-01T00:00:00"),
            active(row("c-quiet", Some("A")), "2026-10-02T00:00:00"),
            waiting(active(row("c-waiting", Some("A")), "2026-09-01T00:00:00")),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["A", "c-waiting", "c-quiet", "B"]);
    }

    #[test]
    fn a_working_row_does_not_buy_attention() {
        // `working` 进 crew 计数，但不进排序键：一次长轮次不该把行顶到
        // 「有人等你」前面。
        let rows = [
            working(active(row("running", None), "2026-10-01T00:00:00")),
            active(waiting(row("asking", None)), "2026-09-01T00:00:00"),
        ];
        let out = build_forest(&rows);
        assert_eq!(order(&rows, &out), ["asking", "running"]);
    }

    #[test]
    fn duplicate_ids_keep_a_total_order() {
        // 畸形输入（同一 id 两行）：id 并列后按输入下标兜底，不随排序算法漂移。
        let rows = [row("dup", None), row("dup", None), row("a", None)];
        let forest = Forest::build(&rows);
        let indices: Vec<usize> = forest.tree().iter().map(|e| e.index).collect();
        assert_eq!(indices, [2, 0, 1]);
    }

    // ── crew 聚合 ────────────────────────────────────────────────

    #[test]
    fn crew_counts_every_descendant_including_dormant_ones() {
        let rows = [
            row("P", None),
            working(row("w1", Some("P"))),
            inactive(row("d1", Some("P"))),
            waiting(active(row("g", Some("w1")), "2026-10-05T00:00:00")),
            inactive(row("gd", Some("d1"))),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(
            forest.crew(0),
            Crew {
                descendants: 4,
                waiting: 1,
                working: 1,
                activity: Some(micros("2026-10-05T00:00:00")),
            }
        );
        // 中间节点只数自己的后代。
        assert_eq!(
            forest.crew(1),
            Crew {
                descendants: 1,
                waiting: 1,
                working: 0,
                activity: Some(micros("2026-10-05T00:00:00")),
            }
        );
        // dormant 分支照样聚合（d1 → gd）。
        assert_eq!(forest.crew(2).descendants, 1);
        assert_eq!(forest.crew(2).waiting, 0);
    }

    #[test]
    fn crew_of_a_leaf_is_empty_but_still_carries_its_activity() {
        let rows = [active(row("leaf", None), "2026-10-05T00:00:00")];
        let forest = Forest::build(&rows);
        assert_eq!(
            forest.crew(0),
            Crew {
                descendants: 0,
                waiting: 0,
                working: 0,
                activity: Some(micros("2026-10-05T00:00:00")),
            }
        );
        // 越界 = 空聚合（调用方只会传自己拿到的下标）。
        assert_eq!(forest.crew(9), Crew::default());
    }

    #[test]
    fn crew_activity_is_the_sort_key_and_ignores_unparsable_times() {
        let rows = [
            row("P", None),
            active(row("old", Some("P")), "2026-09-01T00:00:00"),
            active(row("new", Some("P")), "2026-10-01T00:00:00"),
            active(row("garbage", Some("P")), "not-a-time"),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(forest.crew(0).activity, Some(micros("2026-10-01T00:00:00")));
    }

    // ── 子树活跃的来源行 ────────────────────────────────────────

    #[test]
    fn the_activity_source_row_is_the_one_the_subtree_max_came_from() {
        let rows = [
            active(row("P", None), "2026-10-01T00:00:00"),
            active(row("old", Some("P")), "2026-09-01T00:00:00"),
            row("mid", Some("P")),
            active(row("new", Some("mid")), "2026-10-05T00:00:00"),
        ];
        let forest = Forest::build(&rows);
        // 叶子 / 自己就是最新动静的那一行 → 自己。
        assert_eq!(forest.activity_of(1), Some(1));
        assert_eq!(forest.activity_of(3), Some(3));
        // 父与中间节点都指向孙那一行（来源行跟着递归走，不是只比直接子）。
        assert_eq!(forest.activity_of(2), Some(3));
        assert_eq!(forest.activity_of(0), Some(3));
        assert_eq!(forest.crew(2).activity, Some(micros("2026-10-05T00:00:00")));
        assert_eq!(forest.crew(0).activity, Some(micros("2026-10-05T00:00:00")));
        // 越界 = `None`（同 crew 的空聚合）。
        assert_eq!(forest.activity_of(99), None);
    }

    #[test]
    fn a_subtree_without_a_usable_time_points_at_the_row_itself() {
        // 整棵子树都解析不了（或没有时间）：消费面仍要显示自己的原文，不能留白。
        let rows = [
            active(row("P", None), "not-a-time"),
            row("child", Some("P")),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(forest.crew(0).activity, None);
        assert_eq!(forest.activity_of(0), Some(0));
        assert_eq!(forest.activity_of(1), Some(1));
    }

    #[test]
    fn the_activity_source_row_does_not_merge_two_spellings_of_one_instant() {
        // 同一微秒、两种书写（手改 metadata 才会出现）：来源是**一行**，不是一张按微秒
        // 反查的表——两个消费面因此拿到同一份原文，也不会互相覆盖。
        let rows = [
            row("P", None),
            active(row("plus-eight", Some("P")), "2026-10-31T16:21:49+08:00"),
            active(row("utc", Some("P")), "2026-10-31T08:21:49"),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(forest.crew(1).activity, forest.crew(2).activity);
        // 并列时取先算到的那一行（同深度按输入序兜底）：再构建一次也不漂移。
        assert_eq!(forest.activity_of(0), Some(1));
        assert_eq!(Forest::build(&rows).activity_of(0), Some(1));
    }

    // ── 存活闭包 + 层投影 ────────────────────────────────────────

    #[test]
    fn a_dormant_parent_stays_kept_for_its_live_child() {
        // 不闭包的话 P 会凭空消失，活的子 c 就升格成根 —— 「层 = 任务」失效。
        let rows = [inactive(row("P", None)), row("c", Some("P"))];
        let forest = Forest::build(&rows);
        let root = forest.level(None, false);
        assert_eq!(ids(&rows, &root), ["P"]);
        assert_eq!(root.dormant_children, 0);
        assert_eq!(ids(&rows, &forest.level(Some("P"), false)), ["c"]);
    }

    #[test]
    fn a_fully_dormant_subtree_is_counted_not_listed() {
        let rows = [
            row("live", None),
            inactive(row("P", None)),
            inactive(row("c", Some("P"))),
        ];
        let forest = Forest::build(&rows);
        let root = forest.level(None, false);
        assert_eq!(ids(&rows, &root), ["live"]);
        assert_eq!(root.dormant_children, 1, "只数直接子（P），不数 P 的子");
    }

    #[test]
    fn a_pinned_dormant_root_keeps_its_place_in_the_default_level() {
        let rows = [
            active(row("live", None), "2026-10-02T00:00:00"),
            pinned(inactive(row("parked", None)), None),
        ];
        let forest = Forest::build(&rows);
        let root = forest.level(None, false);
        assert_eq!(ids(&rows, &root), ["parked", "live"]);
        assert_eq!(root.dormant_children, 0);
    }

    #[test]
    fn include_dormant_lists_everything_and_reports_no_collapse() {
        let rows = [
            active(row("r1", None), "2026-10-01T00:00:00"),
            inactive(row("dormant", None)),
            active(row("r2", None), "2026-10-02T00:00:00"),
        ];
        let forest = Forest::build(&rows);
        let root = forest.level(None, true);
        assert_eq!(ids(&rows, &root), ["r2", "r1", "dormant"]);
        assert_eq!(root.dormant_children, 0);
        // `include_dormant` 只放开过滤、不改顺序：保留集是全量的**子序列**（不是前缀
        // ——dormant 行可以夹在中间）。
        let kept: Vec<&str> = ids(&rows, &forest.level(None, false));
        let all: Vec<&str> = ids(&rows, &root);
        assert_eq!(kept, ["r2", "r1"]);
        let mut rest = all.iter();
        for id in &kept {
            assert!(rest.any(|t| t == id), "{id} 不在全量里或相对顺序变了");
        }
    }

    #[test]
    fn a_level_lists_direct_children_only() {
        let rows = [
            row("P", None),
            row("c1", Some("P")),
            row("c2", Some("P")),
            row("g", Some("c1")),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(ids(&rows, &forest.level(None, false)), ["P"]);
        assert_eq!(ids(&rows, &forest.level(Some("P"), false)), ["c1", "c2"]);
        assert_eq!(ids(&rows, &forest.level(Some("c1"), false)), ["g"]);
        assert!(forest.level(Some("g"), false).rows.is_empty());
    }

    #[test]
    fn an_unknown_scope_is_an_empty_level() {
        let rows = [row("P", None), row("c", Some("P"))];
        let forest = Forest::build(&rows);
        assert_eq!(forest.level(Some("gone"), false), Level::default());
        assert_eq!(forest.level(Some("gone"), true), Level::default());
    }

    #[test]
    fn ghost_parents_and_cycles_are_roots_in_the_level_view() {
        let rows = [
            row("x", Some("ghost")),
            row("a", Some("b")),
            row("b", Some("a")),
        ];
        let forest = Forest::build(&rows);
        let root = forest.level(None, false);
        assert_eq!(ids(&rows, &root), ["a", "b", "x"]);
        assert_eq!(root.dormant_children, 0);
        // 幽灵父不是 scope（它不在集合里）。
        assert_eq!(forest.level(Some("ghost"), true), Level::default());
    }

    #[test]
    fn parent_of_follows_the_effective_parent_chain() {
        let rows = [
            row("P", None),
            row("c", Some("P")),
            row("g", Some("c")),
            row("cyc-a", Some("cyc-b")),
            row("cyc-b", Some("cyc-a")),
            row("orphan", Some("ghost")),
        ];
        let forest = Forest::build(&rows);
        assert_eq!(forest.parent_of(0), None);
        assert_eq!(forest.parent_of(1), Some(0));
        assert_eq!(forest.parent_of(2), Some(1));
        // 环断开 → 环上节点是根（面包屑不会绕圈）。
        assert_eq!(forest.parent_of(3), None);
        assert_eq!(forest.parent_of(4), None);
        assert_eq!(forest.parent_of(5), None);
        assert_eq!(forest.parent_of(99), None);
    }

    #[test]
    fn the_levels_partition_the_kept_rows_in_total_order() {
        let rows = [
            active(row("root-a", None), "2026-10-01T00:00:00"),
            inactive(row("parked", None)),
            working(row("kid-a", Some("root-a"))),
            inactive(row("kid-dead", Some("root-a"))),
            waiting(active(row("grand", Some("kid-a")), "2026-10-05T00:00:00")),
            active(row("root-b", None), "2026-10-09T00:00:00"),
        ];
        let forest = Forest::build(&rows);
        let collected = collect_levels(&rows, &forest);
        assert_eq!(collected, ["root-a", "kid-a", "grand", "root-b"]);
        // 无重复、无孤儿。
        let unique: HashSet<&str> = collected.iter().copied().collect();
        assert_eq!(
            unique.len(),
            collected.len(),
            "duplicate rows: {collected:?}"
        );
        // 相对顺序 = 全序的限制（collected 是全序的子序列）。
        let total = order(&rows, forest.tree());
        let mut rest = total.iter();
        for id in &collected {
            assert!(rest.any(|t| t == id), "{id} 不在全序里或相对顺序变了");
        }
    }

    #[test]
    fn the_projection_ignores_the_input_order() {
        let rows = [
            active(row("root", None), "2026-10-01T00:00:00"),
            pinned(inactive(row("parked", None)), Some("2026-10-02T00:00:00")),
            inactive(row("gone", Some("root"))),
            waiting(row("ask", Some("root"))),
            working(row("work", Some("root"))),
            row("orphan", Some("ghost")),
        ];
        let mut reversed = rows.to_vec();
        reversed.reverse();
        let forward = Forest::build(&rows);
        let backward = Forest::build(&reversed);

        let tree_of = |rows: &[TreeRow<'_>], forest: &Forest| -> Vec<(String, String)> {
            forest
                .tree()
                .iter()
                .map(|e| (rows[e.index].id.to_string(), e.prefix.clone()))
                .collect()
        };
        assert_eq!(tree_of(&rows, &forward), tree_of(&reversed, &backward));
        assert_eq!(
            collect_levels(&rows, &forward),
            collect_levels(&reversed, &backward)
        );
        for (i, r) in rows.iter().enumerate() {
            let j = reversed
                .iter()
                .position(|other| other.id == r.id)
                .expect("fixture ids are unique");
            assert_eq!(forward.crew(i), backward.crew(j), "crew of {}", r.id);
        }
    }
}
