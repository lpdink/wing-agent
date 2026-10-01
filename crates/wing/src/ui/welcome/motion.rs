//! 海鸥的动作规划器 —— 纯 deadline 状态机，无定时器、无 I/O。
//!
//! 语义借鉴 dsh 的像素鲸鱼（`whaleIdle.ts`）：**每个动作族是独立的一条平面**
//! （眨眼 / 抖翅 / 跳各管各的 deadline），所以"眨眼落在抖翅中途"是天然成立的，
//! 不需要一个总仲裁器。与鲸鱼不同的是姿态只有两档：
//!
//! * **待机**（agent 闲）：站姿 chibi，偶尔眨眼 / 抖翅 / 跳；
//! * **干活**（turn 进行中）：切成飞行扇翅循环 —— "它在飞" = "它在干活"。
//!
//! 节奏是写死的常数加**确定性 jitter**（由内部计数器派生，不碰 RNG）：
//! 同一进程里间隔不机械重复，但测试可以精确复算。

/// 飞行扇翅循环的帧数（见 [`super::art::FLY_0`]..）。
pub const FLY_FRAMES: usize = 6;

/// 扇翅每帧停留（ms）。6 帧一轮 ≈ 0.54s，够"扑"但不 frantic。
const FLY_HOLD_MS: u64 = 90;

/// 眨眼闭眼时长。
const BLINK_HOLD_MS: u64 = 140;
/// 眨眼间隔基准 / jitter 上限。
const BLINK_GAP_MS: u64 = 4200;
const BLINK_GAP_JITTER_MS: u64 = 2400;

/// 抖翅序列每步停留；序列 = [1, 2, 1]。
const FLUTTER_HOLD_MS: u64 = 90;
const FLUTTER_GAP_MS: u64 = 6500;
const FLUTTER_GAP_JITTER_MS: u64 = 3000;

/// 跳起每帧停留；序列 = [1, 1]（两帧离地）。
const HOP_HOLD_MS: u64 = 130;
const HOP_GAP_MS: u64 = 12000;
const HOP_GAP_JITTER_MS: u64 = 5000;

/// 长时间没被 advance（窗口挂起、机器睡眠）时的迭代护栏：
/// 直接把 deadline 跳到 now 之后，而不是补算几百轮。
const CATCHUP_GUARD: u32 = 64;

/// 站姿的当前帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerchedFrame {
    Idle,
    Blink,
    Flutter1,
    Flutter2,
}

/// 欢迎屏海鸥的这一帧该画什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pose {
    /// 待机：站姿 chibi。`hop` = 离地（渲染时整帧上抬一行）。
    Perched { frame: PerchedFrame, hop: bool },
    /// 干活：飞行扇翅，`frame` ∈ 0..[`FLY_FRAMES`]。
    Flying { frame: usize },
}

/// 一条动作族平面：序列下标（-1 = 休息）+ 两个 deadline。
#[derive(Debug, Clone, Copy)]
struct Limb {
    /// 序列下标；-1 = 休息。
    step: i32,
    /// 当前步的到期时刻。
    hold_until: u64,
    /// 下一轮起跳时刻（休息中才有意义）。
    next_pass_at: u64,
}

impl Limb {
    fn resting(at: u64) -> Self {
        Self {
            step: -1,
            hold_until: 0,
            next_pass_at: at,
        }
    }

    /// 把到期的推进都吃掉。`seq` 是动作帧序列（值即对外暴露的 step 值）。
    fn advance(&mut self, now: u64, seq: &[i32], hold: u64, gap: u64, jitter: u64, rnd: &mut u64) {
        let mut guard = 0;
        while self.step >= 0 && now >= self.hold_until && guard < CATCHUP_GUARD {
            guard += 1;
            if self.step as usize + 1 >= seq.len() {
                self.step = -1;
                self.next_pass_at = now.saturating_add(gap + jittered(jitter, rnd));
            } else {
                self.step += 1;
                self.hold_until = self.hold_until.saturating_add(hold);
            }
        }
        if guard >= CATCHUP_GUARD && self.step >= 0 {
            self.step = -1;
            self.next_pass_at = now.saturating_add(gap);
        }
        if self.step < 0 && now >= self.next_pass_at {
            self.step = 0;
            self.hold_until = now.saturating_add(hold);
        }
    }

    /// 当前对外 step 值（休息 = 0）。
    fn value(&self, seq: &[i32]) -> i32 {
        if self.step < 0 {
            0
        } else {
            seq[self.step as usize]
        }
    }

    /// 下一次会改变姿态的时刻。
    fn next_due(&self, seq: &[i32]) -> u64 {
        if self.step >= 0 {
            self.hold_until
        } else {
            let _ = seq;
            self.next_pass_at
        }
    }
}

/// 确定性 jitter：`0..jitter` 之间一个由计数器派生的值。
fn jittered(jitter: u64, rnd: &mut u64) -> u64 {
    if jitter == 0 {
        return 0;
    }
    *rnd = rnd
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*rnd >> 33) % jitter
}

const BLINK_SEQ: &[i32] = &[1];
const FLUTTER_SEQ: &[i32] = &[1, 2, 1];
const HOP_SEQ: &[i32] = &[1, 1];

/// 规划器状态。构造时播种各动作族的首次时刻。
#[derive(Debug, Clone)]
pub struct Motion {
    blink: Limb,
    flutter: Limb,
    hop: Limb,
    fly_step: usize,
    fly_hold_until: u64,
    was_working: bool,
    rnd: u64,
}

impl Motion {
    /// `now` / `seed` 都是毫秒刻度（调用方给 `Instant` 差值）。
    pub fn new(now: u64, seed: u64) -> Self {
        let mut rnd = seed.wrapping_mul(2654435761) | 1;
        Self {
            blink: Limb::resting(now + BLINK_GAP_MS + jittered(BLINK_GAP_JITTER_MS, &mut rnd)),
            flutter: Limb::resting(
                now + FLUTTER_GAP_MS + jittered(FLUTTER_GAP_JITTER_MS, &mut rnd),
            ),
            hop: Limb::resting(now + HOP_GAP_MS + jittered(HOP_GAP_JITTER_MS, &mut rnd)),
            fly_step: 0,
            fly_hold_until: 0,
            was_working: false,
            rnd,
        }
    }

    /// 这一时刻该画什么（不改动状态）。
    pub fn pose(&self, now: u64, working: bool) -> Pose {
        if working {
            return Pose::Flying {
                frame: self.fly_step % FLY_FRAMES,
            };
        }
        let frame = if self.blink.value(BLINK_SEQ) > 0 && now < self.blink.hold_until {
            PerchedFrame::Blink
        } else {
            match self.flutter.value(FLUTTER_SEQ) {
                1 => PerchedFrame::Flutter1,
                2 => PerchedFrame::Flutter2,
                _ => PerchedFrame::Idle,
            }
        };
        Pose::Perched {
            frame,
            hop: self.hop.value(HOP_SEQ) > 0,
        }
    }

    /// 吃掉所有到期的推进；`working` 翻转时重新播种对面那档的节奏。
    pub fn advance(&mut self, now: u64, working: bool) {
        if working != self.was_working {
            self.was_working = working;
            if working {
                self.fly_step = 0;
                self.fly_hold_until = now.saturating_add(FLY_HOLD_MS);
            } else {
                // 落回站姿：各动作族从此刻重新排，避免"一落地就连眨三下"。
                self.blink = Limb::resting(
                    now + BLINK_GAP_MS + jittered(BLINK_GAP_JITTER_MS, &mut self.rnd),
                );
                self.flutter = Limb::resting(
                    now + FLUTTER_GAP_MS + jittered(FLUTTER_GAP_JITTER_MS, &mut self.rnd),
                );
                self.hop =
                    Limb::resting(now + HOP_GAP_MS + jittered(HOP_GAP_JITTER_MS, &mut self.rnd));
            }
        }
        if working {
            let mut guard = 0;
            while now >= self.fly_hold_until && guard < CATCHUP_GUARD {
                guard += 1;
                self.fly_step = (self.fly_step + 1) % FLY_FRAMES;
                self.fly_hold_until = self.fly_hold_until.saturating_add(FLY_HOLD_MS);
            }
            if guard >= CATCHUP_GUARD {
                self.fly_hold_until = now.saturating_add(FLY_HOLD_MS);
            }
            return;
        }
        self.blink.advance(
            now,
            BLINK_SEQ,
            BLINK_HOLD_MS,
            BLINK_GAP_MS,
            BLINK_GAP_JITTER_MS,
            &mut self.rnd,
        );
        self.flutter.advance(
            now,
            FLUTTER_SEQ,
            FLUTTER_HOLD_MS,
            FLUTTER_GAP_MS,
            FLUTTER_GAP_JITTER_MS,
            &mut self.rnd,
        );
        self.hop.advance(
            now,
            HOP_SEQ,
            HOP_HOLD_MS,
            HOP_GAP_MS,
            HOP_GAP_JITTER_MS,
            &mut self.rnd,
        );
    }

    /// 下一次会改变姿态的绝对时刻（调用方拿它当 tick 的 deadline）。
    pub fn next_due(&self, working: bool) -> u64 {
        if working {
            return self.fly_hold_until;
        }
        self.blink
            .next_due(BLINK_SEQ)
            .min(self.flutter.next_due(FLUTTER_SEQ))
            .min(self.hop.next_due(HOP_SEQ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_perched_and_idle() {
        let m = Motion::new(0, 7);
        assert_eq!(
            m.pose(0, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                hop: false
            }
        );
    }

    #[test]
    fn working_flies_and_cycles() {
        let mut m = Motion::new(0, 7);
        m.advance(0, true);
        assert_eq!(m.pose(0, true), Pose::Flying { frame: 0 });
        m.advance(FLY_HOLD_MS, true);
        assert_eq!(m.pose(FLY_HOLD_MS, true), Pose::Flying { frame: 1 });
        m.advance(FLY_HOLD_MS * 6, true);
        assert_eq!(
            m.pose(FLY_HOLD_MS * 6, true),
            Pose::Flying { frame: 0 },
            "循环回第 0 帧"
        );
    }

    #[test]
    fn blink_fires_and_clears() {
        let mut m = Motion::new(0, 7);
        let due = m.next_due(false);
        m.advance(due, false);
        assert_eq!(
            m.pose(due, false),
            Pose::Perched {
                frame: PerchedFrame::Blink,
                hop: false
            }
        );
        let after = due + BLINK_HOLD_MS + 1;
        m.advance(after, false);
        assert_eq!(
            m.pose(after, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                hop: false
            },
            "闭眼窗口过了就睁眼"
        );
    }

    #[test]
    fn next_due_always_moves_forward() {
        let mut m = Motion::new(0, 3);
        let mut now = 0;
        for _ in 0..200 {
            let due = m.next_due(false);
            assert!(due > now, "deadline 必须严格前进：now={now} due={due}");
            now = due;
            m.advance(now, false);
        }
    }

    #[test]
    fn landing_reseeds_the_perched_rhythm() {
        let mut m = Motion::new(0, 3);
        m.advance(0, true);
        m.advance(1000, true);
        m.advance(1000, false);
        // 刚落回站姿不该立刻眨眼 / 抖翅。
        assert_eq!(
            m.pose(1000, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                hop: false
            }
        );
        assert!(m.next_due(false) > 1000 + BLINK_GAP_MS / 2);
    }

    #[test]
    fn long_sleep_does_not_spin() {
        let mut m = Motion::new(0, 3);
        // 挂起一小时后回来：一次 advance 就该收敛，姿态合法。
        m.advance(3_600_000, false);
        match m.pose(3_600_000, false) {
            Pose::Perched { .. } => {}
            other => panic!("待机姿态非法：{other:?}"),
        }
        assert!(m.next_due(false) > 3_600_000);
    }
}
