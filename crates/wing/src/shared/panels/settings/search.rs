//! 搜索：匹配域、命中集 / 祖先集，以及行内高亮片段。
//!
//! 匹配是**子串**（不做模糊），大小写不敏感，域 = `path` / `title` / `doc` / `notes[]` /
//! `choices[].value` / `choices[].doc`（design §14.2）。命中集按 `(根, 模板路径)` 预计算；
//! [`super::flatten`] 用「命中 ⇒ 可见」「祖先 ⇒ 可见且展开」「命中结构节点的直接子节点 ⇒ 可见」
//! 三条规则决定搜索态的树形。
//!
//! 这里的 `SearchFilter` 是纯数据 + 纯查询；面板侧的 [`SearchState`] 才装「展开快照 / 光标快照」
//! 这些会话状态。

use std::collections::HashSet;
use std::ops::Range;

use wing_api_client::models::SettingNode;

use super::doc::Root;
use crate::shared::doc_edit::ancestors;
use crate::shared::doc_edit::path_is_within;

/// 一次搜索的匹配结果（跨根）。
#[derive(Debug, Clone)]
pub struct SearchFilter {
    needle: String,
    hits: HashSet<(Root, String)>,
    ancestor_paths: HashSet<(Root, String)>,
}

impl SearchFilter {
    /// 扫两个目录求命中集与祖先集；空白 query → 空结果。
    pub fn new(query: &str, roots: &[(Root, &SettingNode)]) -> Self {
        let needle = query.to_lowercase();
        let mut hits = HashSet::new();
        if !needle.trim().is_empty() {
            for (root, catalog) in roots {
                for child in &catalog.children {
                    collect_hits(child, *root, &needle, &mut hits);
                }
            }
        }
        let mut ancestor_paths = HashSet::new();
        for (root, path) in &hits {
            // 根头行也要算「有命中在下面」——搜索跨根时两个根都要展开。
            ancestor_paths.insert((*root, String::new()));
            for ancestor in ancestors(path) {
                if !ancestor.is_empty() {
                    ancestor_paths.insert((*root, ancestor));
                }
            }
        }
        Self {
            needle,
            hits,
            ancestor_paths,
        }
    }

    /// 命中节点数（标题栏 `搜索: x（3 命中）`）。
    pub fn hits(&self) -> usize {
        self.hits.len()
    }

    /// 这个模板路径自己是否命中。
    pub(crate) fn is_hit(&self, root: Root, path: &str) -> bool {
        self.hits.contains(&(root, path.to_string()))
    }

    /// 这个模板路径是否是某个命中项的**严格祖先**（要可见且强制展开）。
    pub(crate) fn is_ancestor(&self, root: Root, path: &str) -> bool {
        self.ancestor_paths.contains(&(root, path.to_string()))
    }

    /// 落在某个分组（= 若干顶层成员）里的命中数（左栏徽标 / 自动跳组）。
    ///
    /// 命中集是按**模板路径**记的（`providers[].api_key`），所以判定用「首段 ∈ 成员」。
    pub(crate) fn hits_under(&self, root: Root, members: &[String]) -> usize {
        self.hits
            .iter()
            .filter(|(hit_root, path)| {
                *hit_root == root && members.iter().any(|member| path_is_within(member, path))
            })
            .count()
    }

    /// `label` 里 query 的出现位置（字节区间，大小写不敏感，互不重叠）。
    pub(crate) fn find_spans(&self, label: &str) -> Vec<Range<usize>> {
        find_spans(label, &self.needle)
    }
}

fn collect_hits(node: &SettingNode, root: Root, needle: &str, hits: &mut HashSet<(Root, String)>) {
    if node_matches(node, needle) {
        hits.insert((root, node.path.clone()));
    }
    if let Some(element) = node.element.as_deref() {
        collect_hits(element, root, needle, hits);
    }
    if let Some(variants) = node.variants.as_deref() {
        for variant in variants {
            collect_hits(variant, root, needle, hits);
        }
    }
    for child in &node.children {
        collect_hits(child, root, needle, hits);
    }
}

fn node_matches(node: &SettingNode, needle: &str) -> bool {
    let contains = |text: &str| text.to_lowercase().contains(needle);
    contains(&node.path)
        || contains(&node.title)
        || contains(&node.doc)
        || node.notes.iter().any(|note| contains(note))
        || node
            .choices
            .iter()
            .any(|choice| contains(&choice.value) || choice.doc.as_deref().is_some_and(contains))
}

/// 大小写不敏感的子串定位（按字符对齐，返回原文的字节区间）。
fn find_spans(haystack: &str, needle: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return spans;
    }
    let chars: Vec<(usize, char)> = haystack.char_indices().collect();
    let mut i = 0;
    while i + needle_chars.len() <= chars.len() {
        let matched = chars[i..i + needle_chars.len()]
            .iter()
            .zip(&needle_chars)
            .all(|((_, c), n)| c.to_lowercase().eq(n.to_lowercase()));
        if matched {
            let start = chars[i].0;
            let end = chars
                .get(i + needle_chars.len())
                .map_or(haystack.len(), |(byte, _)| *byte);
            spans.push(start..end);
            i += needle_chars.len();
        } else {
            i += 1;
        }
    }
    spans
}

/// 面板侧的搜索会话：query + 进入搜索时的展开 / 光标 / 分组快照（Esc 恢复用）。
#[derive(Debug, Clone)]
pub(crate) struct SearchState {
    query: String,
    snapshot: HashSet<(Root, String)>,
    cursor_snapshot: Option<(Root, String)>,
    group_snapshot: usize,
}

impl SearchState {
    pub(crate) fn new(
        expanded: &HashSet<(Root, String)>,
        cursor: Option<(Root, String)>,
        group: usize,
    ) -> Self {
        Self {
            query: String::new(),
            snapshot: expanded.clone(),
            cursor_snapshot: cursor,
            group_snapshot: group,
        }
    }

    /// 进入搜索前选中的分组（搜索会为了「跳到有命中的组」挪动它，Esc 要还回去）。
    pub(crate) fn group_snapshot(&self) -> usize {
        self.group_snapshot
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn push(&mut self, c: char) {
        self.query.push(c);
    }

    pub(crate) fn backspace(&mut self) {
        self.query.pop();
    }

    pub(crate) fn clear(&mut self) {
        self.query.clear();
    }

    /// 退出搜索要还回去的展开集（搜索前的快照）。
    pub(crate) fn snapshot(&self) -> &HashSet<(Root, String)> {
        &self.snapshot
    }

    /// 退出搜索要还回去的光标（搜索前那一行，可能已经不存在）。
    pub(crate) fn cursor_snapshot(&self) -> Option<(Root, String)> {
        self.cursor_snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::test_support as fx;
    use wing_api_client::models::SettingKind;

    fn filter(query: &str) -> SearchFilter {
        let catalog = fx::sample_catalog();
        SearchFilter::new(query, &[(Root::Gateway, &catalog)])
    }

    #[test]
    fn matching_covers_path_title_doc_notes_and_choices() {
        assert!(filter("api_key").is_hit(Root::Gateway, "providers[].api_key"));
        assert!(
            filter("port").is_hit(Root::Gateway, "gateway.port"),
            "path 命中"
        );
        assert!(
            filter("Anthropic").is_hit(Root::Gateway, "providers[].protocol"),
            "choices[].doc 命中"
        );
        assert!(
            filter("anthropic").is_hit(Root::Gateway, "providers[].protocol"),
            "大小写不敏感"
        );
        assert!(
            filter("的说明").is_hit(Root::Gateway, "gateway.port"),
            "doc 命中"
        );
        assert!(
            filter("port").is_hit(Root::Gateway, "gateway.port"),
            "title/key 命中"
        );
        assert_eq!(filter("nothing-here").hits(), 0);
        assert_eq!(filter("").hits(), 0, "空 query 无命中");
    }

    #[test]
    fn notes_are_part_of_the_match_domain() {
        let mut node = fx::str_field("f");
        node.notes = vec!["多行详解里的关键词：视差".into()];
        let catalog = fx::root(vec![node]);
        let f = SearchFilter::new("视差", &[(Root::Gateway, &catalog)]);
        assert!(f.is_hit(Root::Gateway, "f"));
    }

    #[test]
    fn ancestors_are_all_template_prefixes_of_hits() {
        let f = filter("api_key");
        assert!(f.is_ancestor(Root::Gateway, "providers"));
        assert!(f.is_ancestor(Root::Gateway, "providers[]"));
        assert!(!f.is_ancestor(Root::Gateway, "gateway"));
        assert!(
            !f.is_ancestor(Root::Gateway, "providers[].api_key"),
            "自己不和自己算祖先"
        );
    }

    #[test]
    fn hits_are_root_scoped() {
        let gateway = fx::sample_catalog();
        let interface = fx::root(vec![fx::str_field("api_key")]);
        let f = SearchFilter::new(
            "api_key",
            &[(Root::Gateway, &gateway), (Root::Interface, &interface)],
        );
        assert!(f.is_hit(Root::Gateway, "providers[].api_key"));
        assert!(f.is_hit(Root::Interface, "api_key"));
        assert_eq!(f.hits(), 2, "跨根计数");
    }

    #[test]
    fn spans_are_byte_ranges_over_the_original_label() {
        let f = filter("port");
        let spans = f.find_spans("gateway.port 端口");
        assert_eq!(spans.len(), 1);
        assert_eq!(&"gateway.port 端口"[spans[0].clone()], "port");
        // 大小写不敏感 + 多个命中。
        let f = filter("ab");
        let spans = f.find_spans("ABab");
        assert_eq!(spans.len(), 2);
        assert_eq!(&"ABab"[spans[0].clone()], "AB");
        assert_eq!(&"ABab"[spans[1].clone()], "ab");
        // 中文标签（多字节）。
        let f = filter("中文");
        let spans = f.find_spans("x中文y");
        assert_eq!(&"x中文y"[spans[0].clone()], "中文");
    }

    #[test]
    fn spans_skip_overlapping_matches() {
        let f = filter("aa");
        assert_eq!(f.find_spans("aaa").len(), 1);
        assert_eq!(f.find_spans("aaaa").len(), 2);
    }

    #[test]
    fn search_state_snapshots_expanded_and_cursor() {
        let mut expanded = HashSet::new();
        expanded.insert((Root::Gateway, "providers".to_string()));
        let mut state = SearchState::new(&expanded, Some((Root::Gateway, "gateway".into())), 3);
        state.push('p');
        state.push('o');
        assert_eq!(state.query(), "po");
        state.backspace();
        assert_eq!(state.query(), "p");
        state.clear();
        assert_eq!(state.query(), "");
        assert!(
            state
                .snapshot()
                .contains(&(Root::Gateway, "providers".to_string()))
        );
        assert_eq!(
            state.cursor_snapshot(),
            Some((Root::Gateway, "gateway".into()))
        );
        assert_eq!(state.group_snapshot(), 3);
    }

    #[test]
    fn unknown_kind_nodes_still_match_on_metadata() {
        let node = fx::node("mystery", SettingKind::Unknown("weird".into()));
        let catalog = fx::root(vec![node]);
        let f = SearchFilter::new("mystery", &[(Root::Gateway, &catalog)]);
        assert!(f.is_hit(Root::Gateway, "mystery"));
    }
}
