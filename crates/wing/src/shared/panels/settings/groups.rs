//! 左列锚点：业务分组（[`SettingGroup`]）→ 面板的导航单元。
//!
//! 分组的**声明**在后端（`config/groups.py` → `GET /api/settings/schema` 的 `groups[]`）
//! 与 Rust 侧的 Interface 根（`config/catalog.rs::interface_groups()`）；本模块只做三件
//! 纯函数的事：
//!
//! 1. 把两份声明拼成一张**有序**锚点表（Gateway 的组在前、Interface 的组在后）；
//! 2. 把成员收敛成 catalog 里真实存在的 root 子节点（按声明序，幻影成员丢掉）；
//! 3. 老网关兜底：没有 `groups[]` 时按 root 子节点的 `section` 推导（[`derive_groups`]）。
//!    这条**策略**住在这里而不是协议镜像层（`wing-api-client`）：那一层只镜像后端发的东西，
//!    "后端没发时前端怎么办"是面板自己的事。
//!
//! 前端**不许**硬编码组名 / 顺序 / 成员：这张表就是左列的全部事实。

use wing_api_client::models::SettingGroup;
use wing_api_client::models::SettingNode;

use super::doc::Root;

/// 一个左列锚点（= 一个业务分组）。
///
/// **身份是 `(root, id)`**，不是 `id`：Gateway 的组由后端声明、Interface 的由 Rust 侧声明，
/// 两个命名空间互不知情（后端哪天加一个叫 `interface` 的组也不该锚到错的栏）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupAnchor {
    /// 这一组属于哪个根（决定右栏读哪份文档 / 哪棵目录）。
    pub root: Root,
    /// 稳定标识（后端 `groups[].id`；兜底推导时是合成 id）。
    pub id: String,
    /// 显示名。
    pub title: String,
    /// 一行说明（右栏的组头 / 锚点详情）。
    pub doc: String,
    /// 成员 = catalog root 直接子节点的 `key`，**按声明序**，且都真实存在。
    pub members: Vec<String>,
}

impl GroupAnchor {
    /// 身份匹配（见结构体 doc）。
    pub(crate) fn is(&self, root: Root, id: &str) -> bool {
        self.root == root && self.id == id
    }
}

/// 锚点的只读投影（08 渲染左列用；徽标数由面板现算）。
///
/// 只带左列**画得出来**的东西：组头说明与所属根都从 `SettingsPanel::group()` 拿，
/// 在这里再放一份就是两份真相。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorView {
    pub title: String,
    /// 光标在这一项上。
    pub selected: bool,
    /// 组内有未保存的改动。
    pub dirty: bool,
    /// 组内的问题条数（0 = 不画徽标）。
    pub problems: usize,
    /// 搜索态下组内的命中数（`None` = 不在搜索）。
    pub hits: Option<usize>,
}

/// 拼出锚点表。
///
/// `interface` = `Some((catalog, groups))` 时追加 Interface 根的组（没有注入 Interface 根
/// 就没有那个锚点，`s` 也不会去写那个文件）。
pub(crate) fn build_anchors(
    gateway_root: &SettingNode,
    gateway_groups: &[SettingGroup],
    interface: Option<(&SettingNode, &[SettingGroup])>,
) -> Vec<GroupAnchor> {
    let mut anchors = anchors_of(Root::Gateway, gateway_root, gateway_groups);
    if let Some((catalog, groups)) = interface {
        anchors.extend(anchors_of(Root::Interface, catalog, groups));
    }
    anchors
}

/// 一个根的分组 → 锚点（成员收敛到真实存在的 root 子节点，顺序 = 声明序）。
fn anchors_of(root: Root, catalog: &SettingNode, groups: &[SettingGroup]) -> Vec<GroupAnchor> {
    let declared = if groups.is_empty() {
        // 老网关不发 groups[]：按 section 推导（连续的同一 section 合成一组）。
        let prefix = match root {
            Root::Gateway => "gateway:",
            Root::Interface => "interface:",
        };
        derive_groups(catalog, prefix)
    } else {
        groups.to_vec()
    };

    let mut anchors: Vec<GroupAnchor> = Vec::new();
    let mut covered: Vec<String> = Vec::new();
    for group in &declared {
        let members: Vec<String> = catalog
            .children
            .iter()
            .filter(|child| group.members.contains(&child.key))
            .map(|child| child.key.clone())
            .collect();
        if members.is_empty() {
            continue; // 全是幻影成员：这个锚点点开是空的，不如不出现
        }
        covered.extend(members.iter().cloned());
        anchors.push(GroupAnchor {
            root,
            id: group.id.clone(),
            title: group.title.clone(),
            doc: group.doc.clone(),
            members,
        });
    }

    // 没被任何组认领的顶层键（后端门禁保证不会发生；真发生了也不能让它在界面里消失）。
    let orphans: Vec<String> = catalog
        .children
        .iter()
        .map(|child| child.key.clone())
        .filter(|key| !covered.contains(key))
        .collect();
    if !orphans.is_empty() {
        anchors.push(GroupAnchor {
            root,
            id: format!("{root:?}:ungrouped").to_lowercase(),
            title: root.label().to_string(),
            doc: String::new(),
            members: orphans,
        });
    }
    anchors
}

/// 老网关兼容：没有 `groups[]` 时，从 root 直接子节点的 `section` 推导分组。
///
/// 连续的同一 `section` 合成一组（顺序 = 声明序）；没有 `section` 的节点各自成组
/// （标题回落键名）。它只是"别把界面画塌"的兜底，语义权威始终是后端那张表。
///
/// `id_prefix` 用来隔开两个根的 id 命名空间（`gateway:` / `interface:`）——id 只在
/// 锚点表内部用于"重算后回到同一组"，撞上会锚到错的栏。
pub(crate) fn derive_groups(root: &SettingNode, id_prefix: &str) -> Vec<SettingGroup> {
    let mut groups: Vec<SettingGroup> = Vec::new();
    for child in &root.children {
        let title = child.section.clone().unwrap_or_else(|| child.key.clone());
        if let Some(last) = groups.last_mut()
            && last.title == title
        {
            last.members.push(child.key.clone());
            continue;
        }
        groups.push(SettingGroup {
            id: format!("{id_prefix}{}", groups.len()),
            title,
            doc: child.section_doc.clone().unwrap_or_default(),
            members: vec![child.key.clone()],
        });
    }
    groups
}

/// 这一组的成员节点（右栏要渲染的子树；按声明序，取不到的成员跳过）。
pub(crate) fn member_nodes<'a>(
    catalog: &'a SettingNode,
    anchor: &GroupAnchor,
) -> Vec<&'a SettingNode> {
    anchor
        .members
        .iter()
        .filter_map(|member| catalog.children.iter().find(|child| &child.key == member))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::test_support as fx;

    fn group(id: &str, title: &str, members: &[&str]) -> SettingGroup {
        fx::group(id, title, members)
    }

    // sample_catalog 的顶层键（声明序）：providers / gateway / tools / extra_body

    #[test]
    fn anchors_follow_the_declared_order_and_keep_only_real_members() {
        let catalog = fx::sample_catalog();
        let anchors = anchors_of(
            Root::Gateway,
            &catalog,
            &[
                group("a", "Alpha", &["tools", "providers"]),
                group("b", "Beta", &["gateway", "phantom"]),
            ],
        );
        assert_eq!(
            anchors.iter().map(|a| a.title.clone()).collect::<Vec<_>>(),
            ["Alpha", "Beta", "Gateway"],
            "前两组的顺序 = 分组表；extra_body 没人认领 ⇒ 兜底锚点"
        );
        // 成员按 catalog 声明序（不是分组表里的顺序）：右栏的顺序 = 文件里的顺序。
        assert_eq!(anchors[0].members, ["providers", "tools"]);
        assert_eq!(anchors[1].members, ["gateway"], "幻影成员被丢掉");
        assert_eq!(anchors[2].members, ["extra_body"]);
        assert!(anchors.iter().all(|a| a.root == Root::Gateway));
    }

    #[test]
    fn full_coverage_leaves_no_fallback_anchor() {
        let catalog = fx::sample_catalog();
        let anchors = anchors_of(Root::Gateway, &catalog, &fx::sample_groups());
        assert_eq!(
            anchors.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["providers", "net", "misc"]
        );
        assert_eq!(anchors[2].members, ["tools", "extra_body"]);
        assert_eq!(anchors[0].doc, "Providers 的说明");
    }

    #[test]
    fn a_group_of_only_phantom_members_does_not_become_an_anchor() {
        let catalog = fx::sample_catalog();
        let anchors = anchors_of(Root::Gateway, &catalog, &[group("a", "Alpha", &["nope"])]);
        assert_eq!(anchors.len(), 1, "只剩兜底锚点");
        assert_eq!(anchors[0].title, "Gateway", "兜底锚点用根名");
        assert_eq!(
            anchors[0].members,
            ["providers", "gateway", "tools", "extra_body"],
            "一个都不许消失"
        );
    }

    #[test]
    fn missing_groups_fall_back_to_sections() {
        let mut catalog = fx::sample_catalog();
        catalog.children[0].section = Some("Providers".into());
        catalog.children[1].section = Some("Net".into());
        catalog.children[2].section = Some("Net".into());
        let anchors = anchors_of(Root::Gateway, &catalog, &[]);
        assert_eq!(
            anchors.iter().map(|a| a.title.clone()).collect::<Vec<_>>(),
            ["Providers", "Net", "extra_body"],
            "连续的同一 section 合成一组；没有 section 的各自成组"
        );
        assert_eq!(anchors[1].members, ["gateway", "tools"]);
    }

    #[test]
    fn interface_anchors_are_appended_after_the_gateway_ones() {
        let gateway = fx::sample_catalog();
        let interface = fx::interface_catalog();
        let anchors = build_anchors(
            &gateway,
            &fx::sample_groups(),
            Some((&interface, &fx::interface_groups())),
        );
        assert_eq!(
            anchors
                .iter()
                .map(|a| (a.root, a.title.clone()))
                .collect::<Vec<_>>(),
            [
                (Root::Gateway, "Providers".to_string()),
                (Root::Gateway, "Net".to_string()),
                (Root::Gateway, "Misc".to_string()),
                (Root::Interface, "Interface".to_string()),
            ]
        );
        assert_eq!(anchors[3].members, ["colors", "layout"]);
    }

    #[test]
    fn member_nodes_resolve_in_declaration_order() {
        let catalog = fx::sample_catalog();
        let anchors = anchors_of(
            Root::Gateway,
            &catalog,
            &[group("a", "Alpha", &["tools", "providers"])],
        );
        let nodes = member_nodes(&catalog, &anchors[0]);
        assert_eq!(
            nodes.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(),
            ["providers", "tools"]
        );
    }

    #[test]
    fn sections_are_derived_into_groups_for_an_old_gateway() {
        let mut catalog = fx::sample_catalog();
        catalog.children[0].section = Some("Providers".into());
        catalog.children[0].section_doc = Some("至少一个".into());
        catalog.children[1].section = Some("Net".into());
        catalog.children[2].section = Some("Net".into());
        let derived = derive_groups(&catalog, "gateway:");
        assert_eq!(
            derived
                .iter()
                .map(|group| (group.id.clone(), group.title.clone(), group.members.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    "gateway:0".into(),
                    "Providers".into(),
                    vec!["providers".to_string()]
                ),
                (
                    "gateway:1".into(),
                    "Net".into(),
                    vec!["gateway".to_string(), "tools".to_string()]
                ),
                (
                    "gateway:2".into(),
                    "extra_body".into(),
                    vec!["extra_body".to_string()]
                ),
            ],
            "连续的同一 section 合成一组；没有 section 的用键名当标题"
        );
        assert_eq!(derived[0].doc, "至少一个", "doc 回落 section_doc");
    }

    #[test]
    fn an_empty_catalog_yields_no_anchor() {
        let catalog = fx::root(vec![]);
        assert!(anchors_of(Root::Gateway, &catalog, &fx::sample_groups()).is_empty());
        assert!(anchors_of(Root::Gateway, &catalog, &[]).is_empty());
    }
}
