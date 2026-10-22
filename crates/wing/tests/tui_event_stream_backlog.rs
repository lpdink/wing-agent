//! 「事件通道被灌满」时的收尾回归：任务停在 `tx.send(...).await` 上（唤醒字节够不到它），
//! 有界等待必须走满上限、记 warn、继续收尾；随后放行一个槽位，任务回到循环顶看到
//! EVENT_STOP 才退出——收尾不依赖「唤醒一定打断 send」。
//!
//! 「已灌满」用**通道水位 + 主动补边沿**做到确定，不用固定 sleep 猜：
//!
//! - Linux 上 crossterm 的 mio 事件源是 **EPOLLET（边沿触发）**，而它一次就绪只读
//!   `TTY_BUFFER_SIZE`（1024B）。一次 >1024B 的写入只会给一次就绪通知，余下字节要等**新
//!   数据**（新边沿）才被读走——泵把整段灌爆转成一次 1800B 写入，于是首轮只出来 ~1024B
//!   的事件（114 条左右）就停住。macOS 实测没有这个停驻（同样输入能一次读完），只有 Linux 会碎。
//! - 因此灌满循环每轮补写一条 9B 事件（新边沿 = 把积压放出来），并用 `Receiver::capacity`
//!   观察进度——没有消费者，容量只会被任务自己的 send 吃掉；归零 ⇒ 它下一次 send 必挂。
//! - 灌满后再握手两轮（各放行一条、容量必须再归零），证明「任务还活着、还在往这个通道
//!   送」——纯水位区分不了「任务死了但队列恰好是满的」。
//! - 最后静默一段，让任务挂上下一次 send（事件已在解析器里，纯内存路径，微秒级；flag
//!   未置，任务没有别的退出口）。
//!
//! 单独一个集成测试文件 = 单独一个进程（fd 0、crossterm 事件源与 EVENT_* 都是进程级全局）。

use std::os::fd::RawFd;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;
use wing::tui::TermEvent;

mod common;

use common::pty::{make_raw, openpty_pair, write_all};

/// 事件通道容量（`spawn_event_stream` 里的 `mpsc::channel(128)`）。
const CHANNEL_CAPACITY: usize = 128;
/// 灌进的事件条数：> 容量（每条 SGR 鼠标 9 字节，200 条 = 1800 字节），留足富余。
const BURST_EVENTS: usize = 200;
/// 前提：灌爆条数必须超过容量，否则「灌满」无从谈起。
const _: () = assert!(BURST_EVENTS > CHANNEL_CAPACITY);
/// 灌满 / 握手的单轮上限：只判「管线还活着」——正常毫秒级到位，给慢 runner 富余。
const FILL_DEADLINE: Duration = Duration::from_secs(10);
/// 补边沿的节奏：每次补写 9B（见文件头），间隔给泵留一轮转发的余地。
const NUDGE_INTERVAL: Duration = Duration::from_millis(5);
/// 灌满后的静默：任务此刻距「挂上下一次 send」只差一次循环，留一段覆盖调度抖动。
const PARK_SETTLE: Duration = Duration::from_millis(50);

const MOUSE: &[u8] = b"\x1b[<0;1;1M";
/// 有界等待「通道容量归零」（= 任务的下一次 send 必挂）：每轮补一条 9B 事件触发新边沿
/// （见文件头），超时即判流水线没在推进。
fn wait_until_full(outer_master: RawFd, rx: &mpsc::Receiver<TermEvent>, what: &str) {
    let deadline = Instant::now() + FILL_DEADLINE;
    let mut nudges = 0usize;
    while rx.capacity() > 0 {
        assert!(
            Instant::now() < deadline,
            "{what}：{FILL_DEADLINE:?} 内通道都没满（len={} capacity={}，容量 {CHANNEL_CAPACITY}，\
             补边沿 {nudges} 次）——泵 / 事件任务没在推进？",
            rx.len(),
            rx.capacity()
        );
        write_all(outer_master, MOUSE);
        nudges += 1;
        std::thread::sleep(NUDGE_INTERVAL);
    }
}

#[test]
fn teardown_survives_an_event_task_parked_on_a_full_channel() {
    // 外层 pty = 「用户终端」；进程 fd 0 先指向它的 slave（脚手架同 stdin_proxy_pty.rs）。
    let (outer_master, outer_slave) = openpty_pair();
    make_raw(outer_slave);
    let saved = unsafe { libc::dup(0) };
    assert_eq!(
        unsafe { libc::dup2(outer_slave, 0) },
        0,
        "fd 0 → outer slave"
    );

    wing::tui::stdin_proxy::install().expect("install stdin proxy");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let entered = rt.enter();
    let mut rx = wing::tui::spawn_event_stream();
    drop(entered);

    // 灌爆：SGR 鼠标一条只有 9 字节，200 条远多于通道容量（Linux 上这段大写入只会触发
    // 一次就绪，后续靠补边沿放出来，见文件头）。
    let burst: Vec<u8> = MOUSE
        .iter()
        .cycle()
        .take(MOUSE.len() * BURST_EVENTS)
        .copied()
        .collect();
    for chunk in burst.chunks(1024) {
        write_all(outer_master, chunk);
    }

    // 1) 等容量归零：第 129 次 send 已经挂在通道上。
    wait_until_full(outer_master, &rx, "灌爆后");

    // 2) 握手两轮：每轮放行一条 → 任务把挂起的 send 补进队列 → 容量再归零。任务据此被
    //    证明还活着、还在往这个通道里送（且解析器里还有富余条数）。
    for round in 1..=2 {
        let _ = rx.blocking_recv();
        wait_until_full(outer_master, &rx, &format!("第 {round} 轮握手后"));
    }

    // 3) 静默：任务从「刚补进一条」走到「挂上下一次 send」（见文件头）。
    std::thread::sleep(PARK_SETTLE);
    let queued = rx.len();
    assert_eq!(
        rx.capacity(),
        0,
        "静默期间没有消费者，容量不该回升（队列里 {queued} 条）"
    );

    // 收尾：任务挂在 send 上，唤醒字节无处着力 → 有界等待必须走满 200ms 上限后才继续。
    let started = Instant::now();
    wing::tui::end_tui_session().expect("end tui session");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(150),
        "任务 parked 在 send 上时必须走满有界等待（实测 {elapsed:?}，队列 {queued} 条）"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "收尾必须有界：{elapsed:?}"
    );

    // 放行一个槽位：挂起的 send 完成，任务回到循环顶 → 看到 EVENT_STOP → 退出。
    let _ = rx.blocking_recv();

    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        drop(rt);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime teardown timed out: the event task never left the parked send");
    closer.join().expect("closer thread");

    assert_eq!(unsafe { libc::isatty(0) }, 1, "fd 0 必须还是 tty");
    unsafe {
        libc::dup2(saved, 0);
        libc::close(saved);
        libc::close(outer_master);
        libc::close(outer_slave);
    }
}
