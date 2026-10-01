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

/// 眨眼闭眼时长（要长到人眼能接住：140ms 在终端里就是一闪而过）。
const BLINK_HOLD_MS: u64 = 180;
/// 眨眼间隔基准 / jitter 上限。
const BLINK_GAP_MS: u64 = 2600;
const BLINK_GAP_JITTER_MS: u64 = 1500;

/// 抖翅序列每步停留；序列 = [1, 2, 1]。
const FLUTTER_HOLD_MS: u64 = 110;
const FLUTTER_GAP_MS: u64 = 3800;
const FLUTTER_GAP_JITTER_MS: u64 = 2000;

/// 跳起每帧停留；序列 = [1, 1]（两帧离地）。
const HOP_HOLD_MS: u64 = 150;

/// 呼吸：常驻平面，序列 [1, 1]（抬起半格再落下）—— 待机"活着"的底噪。
/// 没有它，待机就是每几秒一闪的静态图（用户实测："等了半天才看到动画"）。
const BREATH_HOLD_MS: u64 = 160;
const BREATH_GAP_MS: u64 = 1500;
const BREATH_GAP_JITTER_MS: u64 = 800;

/// 起飞过渡：站姿先扑两下翼再切飞行（`FLUTTER_HOLD × 2`）。
const TAKEOFF_MS: u64 = FLUTTER_HOLD_MS * 2;
/// 落地过渡：切回站姿时先带一跳（`HOP_HOLD × 2`），像踩到地上。
const LAND_MS: u64 = HOP_HOLD_MS * 2;
const HOP_GAP_MS: u64 = 8000;
const HOP_GAP_JITTER_MS: u64 = 4000;

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
    /// 待机：站姿 chibi。`lift` = 上抬的像素行（0 站定 / 1 呼吸半格 / 2 跳起）。
    Perched { frame: PerchedFrame, lift: u8 },
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
const BREATH_SEQ: &[i32] = &[1, 1];

/// 规划器状态。构造时播种各动作族的首次时刻。
#[derive(Debug, Clone)]
pub struct Motion {
    blink: Limb,
    flutter: Limb,
    hop: Limb,
    breath: Limb,
    fly_step: usize,
    fly_hold_until: u64,
    /// 起飞过渡起点（`None` = 不在过渡中；用 Option 而不是 0 哨兵 ——
    /// 开屏时刻就是 0，哨兵会和真值撞）。
    takeoff: Option<u64>,
    /// 落地过渡终点（0 = 不在过渡中）。
    land_until: u64,
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
            breath: Limb::resting(now + BREATH_GAP_MS + jittered(BREATH_GAP_JITTER_MS, &mut rnd)),
            fly_step: 0,
            fly_hold_until: 0,
            takeoff: None,
            land_until: 0,
            was_working: false,
            rnd,
        }
    }

    /// 这一时刻该画什么（不改动状态）。
    pub fn pose(&self, now: u64, working: bool) -> Pose {
        if working {
            // 起飞过渡：先扑两下翼再离地，idle→fly 不硬切。
            if let Some(at) = self.takeoff
                && now < at + TAKEOFF_MS
            {
                let step = (now - at) / FLUTTER_HOLD_MS;
                return Pose::Perched {
                    frame: if step == 0 {
                        PerchedFrame::Flutter1
                    } else {
                        PerchedFrame::Flutter2
                    },
                    lift: 0,
                };
            }
            return Pose::Flying {
                frame: self.fly_step % FLY_FRAMES,
            };
        }
        // 落地过渡：带着一跳踩回站姿。
        if self.land_until > 0 && now < self.land_until {
            return Pose::Perched {
                frame: PerchedFrame::Idle,
                lift: 2,
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
        let lift = if self.hop.value(HOP_SEQ) > 0 {
            2
        } else if self.breath.value(BREATH_SEQ) > 0 {
            1
        } else {
            0
        };
        Pose::Perched { frame, lift }
    }

    /// 吃掉所有到期的推进；`working` 翻转时重新播种对面那档的节奏。
    pub fn advance(&mut self, now: u64, working: bool) {
        if working != self.was_working {
            self.was_working = working;
            if working {
                self.fly_step = 0;
                // 扇翅时钟从离地那一刻起算：扑翼过渡期间不空转。
                self.fly_hold_until = now.saturating_add(TAKEOFF_MS + FLY_HOLD_MS);
                self.takeoff = Some(now);
                self.land_until = 0;
            } else {
                self.takeoff = None;
                self.land_until = now.saturating_add(LAND_MS);
                // 落回站姿：各动作族从此刻重新排，避免"一落地就连眨三下"。
                self.blink = Limb::resting(
                    now + BLINK_GAP_MS + jittered(BLINK_GAP_JITTER_MS, &mut self.rnd),
                );
                self.flutter = Limb::resting(
                    now + FLUTTER_GAP_MS + jittered(FLUTTER_GAP_JITTER_MS, &mut self.rnd),
                );
                self.hop =
                    Limb::resting(now + HOP_GAP_MS + jittered(HOP_GAP_JITTER_MS, &mut self.rnd));
                self.breath = Limb::resting(
                    now + BREATH_GAP_MS + jittered(BREATH_GAP_JITTER_MS, &mut self.rnd),
                );
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
            if let Some(at) = self.takeoff
                && now >= at + TAKEOFF_MS
            {
                self.takeoff = None;
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
        self.breath.advance(
            now,
            BREATH_SEQ,
            BREATH_HOLD_MS,
            BREATH_GAP_MS,
            BREATH_GAP_JITTER_MS,
            &mut self.rnd,
        );
        // 落地窗口过了就收掉：否则 next_due 会一直报一个过去的时刻。
        if self.land_until > 0 && now >= self.land_until {
            self.land_until = 0;
        }
    }

    /// 下一次会改变姿态的绝对时刻（调用方拿它当 tick 的 deadline）。
    pub fn next_due(&self, working: bool) -> u64 {
        if working {
            let due = self.fly_hold_until;
            return match self.takeoff {
                Some(at) => due.min(at + FLUTTER_HOLD_MS).min(at + TAKEOFF_MS),
                None => due,
            };
        }
        let mut due = self
            .blink
            .next_due(BLINK_SEQ)
            .min(self.flutter.next_due(FLUTTER_SEQ))
            .min(self.hop.next_due(HOP_SEQ));
        if self.land_until > 0 {
            due = due.min(self.land_until);
        }
        due
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
                lift: 0
            }
        );
    }

    #[test]
    fn working_flies_and_cycles() {
        let mut m = Motion::new(0, 7);
        m.advance(0, true);
        assert_eq!(
            m.pose(0, true),
            Pose::Perched {
                frame: PerchedFrame::Flutter1,
                lift: 0
            },
            "开工先扑翼"
        );
        m.advance(TAKEOFF_MS, true);
        assert_eq!(m.pose(TAKEOFF_MS, true), Pose::Flying { frame: 0 });
        m.advance(TAKEOFF_MS + FLY_HOLD_MS, true);
        assert_eq!(
            m.pose(TAKEOFF_MS + FLY_HOLD_MS, true),
            Pose::Flying { frame: 1 }
        );
        m.advance(TAKEOFF_MS + FLY_HOLD_MS * 6, true);
        assert_eq!(
            m.pose(TAKEOFF_MS + FLY_HOLD_MS * 6, true),
            Pose::Flying { frame: 0 },
            "循环回第 0 帧"
        );
    }

    #[test]
    fn blink_fires_and_clears() {
        let mut m = Motion::new(0, 7);
        let due = m.next_due(false);
        m.advance(due, false);
        // 呼吸是常驻平面，lift 随时可能是 1 —— 这里只钉眨眼这一族。
        assert!(
            matches!(
                m.pose(due, false),
                Pose::Perched {
                    frame: PerchedFrame::Blink,
                    ..
                }
            ),
            "到点闭眼"
        );
        let after = due + BLINK_HOLD_MS + 1;
        m.advance(after, false);
        assert!(
            !matches!(
                m.pose(after, false),
                Pose::Perched {
                    frame: PerchedFrame::Blink,
                    ..
                }
            ),
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
        // 落地窗口内是"踩地一跳"，窗口过了回标准站姿；各动作族从落地那一刻
        // 重新排 —— 不该立刻眨眼 / 抖翅。
        assert_eq!(
            m.pose(1000, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                lift: 2
            }
        );
        let down = 1000 + LAND_MS;
        m.advance(down, false);
        assert_eq!(
            m.pose(down, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                lift: 0
            }
        );
        assert!(m.next_due(false) > down + BLINK_GAP_MS / 2);
    }

    #[test]
    fn takeoff_bridges_idle_to_fly() {
        let mut m = Motion::new(0, 7);
        m.advance(0, true);
        assert_eq!(
            m.pose(0, true),
            Pose::Perched {
                frame: PerchedFrame::Flutter1,
                lift: 0
            },
            "开工第一帧是扑翼，不是硬切飞行"
        );
        m.advance(FLUTTER_HOLD_MS, true);
        assert_eq!(
            m.pose(FLUTTER_HOLD_MS, true),
            Pose::Perched {
                frame: PerchedFrame::Flutter2,
                lift: 0
            }
        );
        m.advance(TAKEOFF_MS, true);
        assert_eq!(m.pose(TAKEOFF_MS, true), Pose::Flying { frame: 0 });
    }

    #[test]
    fn landing_touches_down_with_a_hop() {
        let mut m = Motion::new(0, 7);
        m.advance(0, true);
        m.advance(TAKEOFF_MS + FLY_HOLD_MS, true);
        let t = TAKEOFF_MS + FLY_HOLD_MS;
        m.advance(t, false);
        assert_eq!(
            m.pose(t, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                lift: 2
            },
            "落地先带一跳"
        );
        m.advance(t + LAND_MS, false);
        assert_eq!(
            m.pose(t + LAND_MS, false),
            Pose::Perched {
                frame: PerchedFrame::Idle,
                lift: 0
            }
        );
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

#[cfg(test)]
mod planner_invariants {
    use super::*;
    #[test]
    fn pose_is_constant_between_deadlines() {
        for seed in 1..40u64 {
            let mut m = Motion::new(0, seed);
            let mut now = 0u64;
            for _ in 0..400 {
                m.advance(now, false);
                let p0 = m.pose(now, false);
                let due = m.next_due(false);
                assert!(due > now, "seed={seed} now={now} due={due}");
                let mut t = now + 1;
                while t < due {
                    let p = m.pose(t, false);
                    assert_eq!(
                        p, p0,
                        "seed={seed} 姿态在 deadline 之间变了：t={t} due={due}"
                    );
                    t += 1;
                }
                now = due;
            }
        }
    }
}
