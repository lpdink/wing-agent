//! Startup tips — the pool behind the welcome block's rotating line and the
//! `/tips` panel.
//!
//! 内容约定（改这个池子时照着来）：
//!
//! * 每条一句话，讲**用户立刻能用上**的操作，不讲实现细节；
//! * 只写稳定的能力（快捷键 / 命令 / 工作流），**不写「本次更新了什么」**——
//!   这正是它取代旧版硬编码 release notes 的原因：敏捷开发下，版本内的
//!   "新特性" 文案天生会过期，而"Esc 能中断" 不会；
//! * 长度上限 [`MAX_TIP_WIDTH`]（显示宽度）：欢迎屏单行可读，`/tips` 清单
//!   保持一行一条；
//! * `group` 只影响 `/tips` 清单的分组展示，欢迎屏随机抽取不看分组。
//!
//! 这里没有文件 / 网络 I/O、不依赖渲染库 —— 中立层，App（`/tips` 的正文）
//! 和 UI（欢迎屏那一行）共用同一份事实来源。

/// 单条 tip 的显示宽度上限（CJK 记 2 列）。
pub const MAX_TIP_WIDTH: usize = 36;

/// `/tips` 清单的分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipGroup {
    Keys,
    Commands,
    Workflow,
}

impl TipGroup {
    /// 清单里的展示顺序。
    pub const ALL: [TipGroup; 3] = [TipGroup::Keys, TipGroup::Commands, TipGroup::Workflow];

    /// 分组标题（清单用）。
    pub fn label(self) -> &'static str {
        match self {
            TipGroup::Keys => "快捷键",
            TipGroup::Commands => "命令",
            TipGroup::Workflow => "工作流",
        }
    }
}

/// 一条提示。
#[derive(Debug, Clone, Copy)]
pub struct Tip {
    pub group: TipGroup,
    pub text: &'static str,
}

/// 提示池。顺序即 `/tips` 清单里的顺序。
pub const TIPS: &[Tip] = &[
    // ── 快捷键 ────────────────────────────────────────────────
    Tip {
        group: TipGroup::Keys,
        text: "Esc 中断回合，Ctrl+C 按两次退出",
    },
    Tip {
        group: TipGroup::Keys,
        text: "Shift+Enter 换行，Enter 发送",
    },
    Tip {
        group: TipGroup::Keys,
        text: "Shift+Enter 看终端，Ctrl+J 一定行",
    },
    Tip {
        group: TipGroup::Keys,
        text: "鼠标拖拽选中，松手即复制",
    },
    Tip {
        group: TipGroup::Keys,
        text: "点击状态栏 ☆ 置顶会话，再点取消",
    },
    Tip {
        group: TipGroup::Keys,
        text: "点击状态栏的会话 ID 可复制它",
    },
    Tip {
        group: TipGroup::Keys,
        text: "PgUp 翻页，Ctrl+End 回到底部",
    },
    Tip {
        group: TipGroup::Keys,
        text: "Ctrl+O 切换思考的详细/简略显示",
    },
    // ── 命令 ─────────────────────────────────────────────────
    Tip {
        group: TipGroup::Commands,
        text: "/model 换模型，面板按 provider 分组",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/context 看上下文占用，/compact 压缩",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/rewind 回到任意一轮，改完重发",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/fork 从某条消息分叉出新会话",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/session <id> 切会话（/ss 同）",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/think 开思考，/yolo 跳过危险确认",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/workdir 换目录，/title 给会话命名",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/skills 看已装技能，/agents 看模板",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/copy 复制最后一条回复，/copy 2 更早",
    },
    Tip {
        group: TipGroup::Commands,
        text: "/clear 只清屏，历史仍在网关",
    },
    // ── 工作流 ────────────────────────────────────────────────
    Tip {
        group: TipGroup::Workflow,
        text: "wing run 后台起任务，wing wait 收尾",
    },
    Tip {
        group: TipGroup::Workflow,
        text: "wing tail 追输出，wing ps 列会话",
    },
    Tip {
        group: TipGroup::Workflow,
        text: "wing start / stop / status 管网关",
    },
];

/// 抽一条 tip —— 同一个 seed 永远同一条（欢迎屏整个进程只抽一次，重绘 / 缩放
/// 不会换掉那一行）。
pub fn pick(seed: u64) -> &'static Tip {
    // `TIPS` 非空由测试钉死；`%` 后再索引不会越界。
    &TIPS[(seed % TIPS.len() as u64) as usize]
}

/// `/tips` 的正文（App 把它作为一条系统消息推给 chat）：按分组列出全部提示
/// —— 欢迎屏只轮换显示其中一条。
pub fn full_text() -> String {
    let mut out = format!("可用提示（{} 条）：", TIPS.len());
    for group in TipGroup::ALL {
        out.push('\n');
        out.push_str(group.label());
        for tip in TIPS.iter().filter(|t| t.group == group) {
            out.push_str("\n  · ");
            out.push_str(tip.text);
        }
    }
    out
}

/// 抽签用的 seed：进程启动时刻的纳秒 —— 与"每次启动换一条" 的语义对齐。
pub fn seed_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn pool_is_not_empty() {
        assert!(TIPS.len() >= 12, "池子太小，/tips 清单会很寒酸");
    }

    #[test]
    fn every_tip_fits_the_welcome_line() {
        for tip in TIPS {
            let width = UnicodeWidthStr::width(tip.text);
            assert!(
                width <= MAX_TIP_WIDTH,
                "tip 超宽（{width} > {MAX_TIP_WIDTH}）：{}",
                tip.text
            );
            assert!(!tip.text.is_empty(), "空 tip");
            assert!(
                !tip.text.ends_with('。'),
                "tips 是短语不是句子，去掉句号：{}",
                tip.text
            );
        }
    }

    #[test]
    fn every_group_has_tips() {
        for group in TipGroup::ALL {
            assert!(
                TIPS.iter().any(|t| t.group == group),
                "分组 {} 是空的",
                group.label()
            );
        }
    }

    #[test]
    fn tips_are_unique() {
        for (i, a) in TIPS.iter().enumerate() {
            for b in &TIPS[i + 1..] {
                assert_ne!(a.text, b.text, "重复的 tip：{}", a.text);
            }
        }
    }

    #[test]
    fn full_text_lists_every_tip_under_its_group() {
        let text = full_text();
        for group in TipGroup::ALL {
            assert!(
                text.contains(group.label()),
                "缺分组标题：{}",
                group.label()
            );
        }
        for tip in TIPS {
            assert!(text.contains(tip.text), "缺提示：{}", tip.text);
        }
    }

    #[test]
    fn pick_is_deterministic_and_in_range() {
        assert_eq!(pick(0).text, TIPS[0].text);
        assert_eq!(pick(7).text, TIPS[7 % TIPS.len()].text);
        // 同一个 seed 永远同一条 —— 欢迎屏重绘不会换行。
        assert_eq!(pick(u64::MAX).text, pick(u64::MAX).text);
    }
}
