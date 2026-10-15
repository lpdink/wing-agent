//! `spawn_event_stream` 全进程只调一次：第二次调用是编程错误，常开断言必须在**任何**
//! 任务/通道创建之前拒绝它（否则旧 handle 被覆盖、或用上不复位的 EVENT_STOP——症状是
//! 键盘永久哑掉且零日志）。
//!
//! 单独一个集成测试文件 = 单独一个进程（EVENT_TASK / EVENT_STOP 是进程级全局；
//! `#[should_panic]` 的 panic 会污染本进程状态，不能与别的测试共用进程）。

#[test]
#[should_panic(expected = "spawn_event_stream must run once per process")]
fn a_second_spawn_is_rejected() {
    // current-thread runtime 只把任务排队、不驱动：第一个事件流不会跑到、碰不到 stdin。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _entered = rt.enter();
    let _first = wing::tui::spawn_event_stream();
    let _second = wing::tui::spawn_event_stream();
}
