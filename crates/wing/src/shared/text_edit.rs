//! 单行编辑器的共享代数：char ↔ byte 与四条编辑原语。
//!
//! 三处单行输入（Ask 面板的自由输入、设置面板的内联编辑器、composer 的行模型）各自持有
//! 「一个 `String` + 一个**字符**下标光标」。字符下标与字节偏移的换算三处共用；四条编辑
//! 操作（插入 / 退格 / 删除 / 移动）两处共用 —— composer 的行模型是多行 + chip 语义
//! （整块粘贴不可拆），走不了这里的单行原语。内容与光标的所有权留在调用方。
//!
//! 住在 `shared` 是因为两边都要用，而 `shared` 不能反过来依赖 `ui`（`tests/layer_guard.rs`）。
//!
//! 契约：`cursor` 是**字符**下标（不是字节），且始终满足 `cursor <= buffer.chars().count()`；
//! 每条操作自己保证这个后置条件（越界只钳位，不 panic）。

/// 第 `char_idx` 个字符的字节偏移（越界钳到字符串末尾）。
pub(crate) fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// 单行编辑的一条操作（相对光标；没有选中区，也没有撤销栈）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditOp {
    /// 在光标处插入一个字符，光标右移一格。
    Insert(char),
    /// 删除光标左侧的字符，光标左移一格（光标在行首时无操作）。
    Backspace,
    /// 删除光标右侧的字符，光标不动（光标在行尾时无操作）。
    Delete,
    /// 光标按字符移动（负 = 左），钳在 `[0, 字符数]`。
    Move(isize),
}

/// 应用一条 [`EditOp`]：`buffer` 是内容，`cursor` 是字符下标。
pub(crate) fn apply(buffer: &mut String, cursor: &mut usize, op: EditOp) {
    match op {
        EditOp::Insert(c) => {
            let byte = char_to_byte(buffer, *cursor);
            buffer.insert(byte, c);
            *cursor += 1;
        }
        EditOp::Backspace => {
            if *cursor == 0 {
                return;
            }
            let from = char_to_byte(buffer, *cursor - 1);
            let to = char_to_byte(buffer, *cursor);
            buffer.replace_range(from..to, "");
            *cursor -= 1;
        }
        EditOp::Delete => {
            if *cursor >= buffer.chars().count() {
                return;
            }
            let from = char_to_byte(buffer, *cursor);
            let to = char_to_byte(buffer, *cursor + 1);
            buffer.replace_range(from..to, "");
        }
        EditOp::Move(delta) => {
            let len = buffer.chars().count();
            *cursor = (*cursor as isize + delta).clamp(0, len as isize) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_to_byte_counts_chars_not_bytes() {
        assert_eq!(char_to_byte("hello", 0), 0);
        assert_eq!(char_to_byte("hello", 3), 3);
        assert_eq!(char_to_byte("你好", 1), 3);
        assert_eq!(char_to_byte("你好", 2), 6);
        assert_eq!(char_to_byte("你好", 9), 6, "越界钳到末尾");
    }

    #[test]
    fn insert_and_delete_work_on_whole_chars() {
        let (mut buf, mut cursor) = ("a你c".to_string(), 1);
        apply(&mut buf, &mut cursor, EditOp::Insert('好'));
        assert_eq!((buf.as_str(), cursor), ("a好你c", 2));
        apply(&mut buf, &mut cursor, EditOp::Backspace);
        assert_eq!((buf.as_str(), cursor), ("a你c", 1));
        apply(&mut buf, &mut cursor, EditOp::Delete);
        assert_eq!((buf.as_str(), cursor), ("ac", 1));
    }

    #[test]
    fn the_edges_are_no_ops() {
        let (mut buf, mut cursor) = (String::new(), 0);
        apply(&mut buf, &mut cursor, EditOp::Backspace);
        apply(&mut buf, &mut cursor, EditOp::Delete);
        assert_eq!((buf.as_str(), cursor), ("", 0));
        apply(&mut buf, &mut cursor, EditOp::Insert('x'));
        assert_eq!((buf.as_str(), cursor), ("x", 1));
    }

    #[test]
    fn move_clamps_instead_of_wrapping() {
        let (mut buf, mut cursor) = ("abc".to_string(), 1);
        apply(&mut buf, &mut cursor, EditOp::Move(-5));
        assert_eq!(cursor, 0);
        apply(&mut buf, &mut cursor, EditOp::Move(99));
        assert_eq!(cursor, 3, "光标停在行尾，不是 99");
        apply(&mut buf, &mut cursor, EditOp::Move(1));
        assert_eq!(cursor, 3);
    }
}
