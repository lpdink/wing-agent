//! stdout 串行化写口。
//!
//! stdio 模式有两个写者——renderer（协议帧）与 stdin pump（control_response）
//! ——它们**必须**串行化：两个并发 `write_all` 会交错、撕裂 JSON 行，编排器
//! 直接解析失败。所有写者共享同一个 [`StdoutSink`]，锁内完成「整行 + 换行 +
//! flush」。
//!
//! 写失败沿用 `println!` 的语义：stdout 写不进去（编排器关了管道）是致命错误
//! 而不是静默丢弃——panic 让进程带非零码退出，编排器看得见。

use std::io::Write;
use std::sync::Mutex;

/// 进程 stdout 的唯一写入口（`Arc<StdoutSink>` 跨任务共享）。
pub struct StdoutSink {
    inner: Mutex<Box<dyn Write + Send>>,
}

impl StdoutSink {
    /// 写真实 stdout。
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// 写任意 `Write`（单测注入捕获件用）。
    pub fn new(inner: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// 写一整行（补 `\n`、flush）。
    pub fn line(&self, text: &str) {
        // panic 过的写者会让锁中毒；不在这里二次 panic（那会把「一个写者写爆
        // stdout」升级成「所有写者一起炸」），取回内部数据继续写。
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(inner, "{text}")
            .and_then(|()| inner.flush())
            .unwrap_or_else(|e| panic!("failed printing to stdout: {e}"));
    }
}

/// 捕获写入字节的测试用 `Write`（与 sink 共享同一份 buffer）。
#[cfg(test)]
#[derive(Clone, Default)]
pub struct CaptureSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for CaptureSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl CaptureSink {
    /// 已写入的文本。
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// 以本捕获件为出口的 sink。
    pub fn sink(&self) -> std::sync::Arc<StdoutSink> {
        std::sync::Arc::new(StdoutSink::new(Box::new(self.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn writes_one_line_per_call() {
        let capture = CaptureSink::default();
        let sink = capture.sink();

        sink.line("{\"a\":1}");
        sink.line("plain");

        assert_eq!(capture.text(), "{\"a\":1}\nplain\n");
    }

    /// 慢写者：放大"两个写者并发时可能交错"的窗口——锁若缺失，8 个线程
    /// 各写 50 行长文本必然出现半行混杂。
    struct SlowSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SlowSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_micros(30));
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn concurrent_writers_never_interleave_lines() {
        const WRITERS: usize = 8;
        const LINES: usize = 50;
        const FILLER: usize = 400;

        let raw = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::new(StdoutSink::new(Box::new(SlowSink(Arc::clone(&raw)))));

        let handles: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for line in 0..LINES {
                        sink.line(&format!("{writer}-{line}-{}", "x".repeat(FILLER)));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let text = String::from_utf8(raw.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), WRITERS * LINES);
        for line in lines {
            // 每行必须是完整的一条记录：前缀 + 填充字符，行内没有第二个写者的字节。
            let (head, filler) = line.split_at(line.len() - FILLER);
            assert_eq!(filler.len(), FILLER);
            assert!(
                filler.chars().all(|c| c == 'x'),
                "torn line (filler corrupted): {}",
                &line[..line.len().min(120)]
            );
            let (writer, seq) = head.trim_end_matches('-').split_once('-').unwrap();
            assert!(
                writer.parse::<usize>().is_ok_and(|w| w < WRITERS)
                    && seq.parse::<usize>().is_ok_and(|s| s < LINES),
                "torn line head: {head}"
            );
        }
    }
}
