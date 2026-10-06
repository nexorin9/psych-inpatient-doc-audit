//! 规模基准的**分段计时**与**峰值内存**探针。
//!
//! ## 为什么耗时不够，还要分段
//!
//! 一个「1000 件批扫花了 800 ms」的数字对现场没有用——信息科问的是
//! 「到底是解析慢、规则慢，还是图慢」。这三段的优化手段完全不同：
//! 解析慢要去改字段抽取，规则慢要去改执行器，图慢要去改索引。
//! 因此本模块把一次批扫拆成**四段**分别计时，并让四段之和与总耗时
//! 对得上（[`SegmentProfile::is_balanced`]），否则「分段」就成了
//! 一组互不相干的数字。
//!
//! ## 为什么峰值内存要单独探
//!
//! 耗时正常但内存爆掉，在批量审查里是真实事故：一次拖进一个病区
//! 一个月的导出包，1000 件解析后的件对象全在内存里。峰值内存不设上限，
//! 现场表现是「转圈转半天最后被系统杀掉」，而日志里什么都看不出来。
//!
//! 探针走 [`getrusage`]（`ru_maxrss`），是**进程级**峰值 RSS：
//! 它记的是操作系统视角的常驻集，不是我们自己估的「对象大概多少字节」，
//! 因此包含了分配器碎片与 serde 的临时缓冲——这些恰恰是大包解析
//! 最先出问题的地方。
//!
//! 平台差异（两条都要处理，否则上限在某个平台上永远是错的）：
//!   - Linux：`ru_maxrss` 单位是 **KiB**；
//!   - macOS：单位是 **字节**。
//!
//! [`peak_rss_bytes`] 已经把两者统一成字节。

use std::time::{Duration, Instant};

/// 批扫主路径的四段。**顺序即执行顺序**，新增段落要同步改
/// [`SegmentProfile::total`]，否则分段之和会对不上总耗时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// 解析：Bundle JSON → 件 / 字段表 / 时间戳集合。
    Parse,
    /// 求值：件级规则 + 跨文书规则 + 豁免 + 去重。
    Review,
    /// 图：文书依赖图建图 + 阻塞闭包 + 正反向对拍。
    Graph,
    /// 写库：件登记 + 审查批次 + 诊断 + 审计 Provenance 落 sqlite。
    Store,
}

impl Stage {
    /// 全部四段，固定顺序。
    pub const ALL: [Stage; 4] = [Stage::Parse, Stage::Review, Stage::Graph, Stage::Store];

    /// 段名（英文标识，进 JSON 与断言信息用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Parse => "parse",
            Stage::Review => "review",
            Stage::Graph => "graph",
            Stage::Store => "store",
        }
    }

    /// 段名（中文，进 profile 报告用）。
    pub fn as_zh(self) -> &'static str {
        match self {
            Stage::Parse => "解析",
            Stage::Review => "求值",
            Stage::Graph => "建图与阻塞传播",
            Stage::Store => "写库",
        }
    }
}

/// 四段耗时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SegmentProfile {
    parse: Duration,
    review: Duration,
    graph: Duration,
    store: Duration,
}

impl SegmentProfile {
    pub fn empty() -> Self {
        Self::default()
    }

    /// 取某一段的累计耗时。
    pub fn get(&self, stage: Stage) -> Duration {
        match stage {
            Stage::Parse => self.parse,
            Stage::Review => self.review,
            Stage::Graph => self.graph,
            Stage::Store => self.store,
        }
    }

    /// 累加某一段。计时器封口时调用。
    pub fn add(&mut self, stage: Stage, d: Duration) {
        let slot = match stage {
            Stage::Parse => &mut self.parse,
            Stage::Review => &mut self.review,
            Stage::Graph => &mut self.graph,
            Stage::Store => &mut self.store,
        };
        *slot += d;
    }

    /// 四段之和。
    pub fn total(&self) -> Duration {
        self.parse + self.review + self.graph + self.store
    }

    /// 某一段占四段之和的百分比（0–100）。四段全为 0 时返回 0。
    pub fn share_of(&self, stage: Stage) -> f64 {
        let total = self.total().as_nanos();
        if total == 0 {
            return 0.0;
        }
        self.get(stage).as_nanos() as f64 / total as f64 * 100.0
    }

    /// 最慢的一段。全 0 时返回 `None`——没有可比的段落时不该硬指一个。
    pub fn slowest(&self) -> Option<Stage> {
        Stage::ALL
            .into_iter()
            .max_by_key(|s| self.get(*s))
            .filter(|s| self.get(*s) > Duration::ZERO)
    }

    /// 供人读的 profile 摘要（`docs/` 下的结果文件与 CLI 输出共用同一份文案）。
    pub fn as_zh(&self) -> String {
        let total = self.total();
        let lines: Vec<String> = Stage::ALL
            .iter()
            .map(|s| {
                format!(
                    "  {:<12} {:>8.1} ms  （{:>5.1}%）",
                    s.as_zh(),
                    self.get(*s).as_secs_f64() * 1000.0,
                    self.share_of(*s)
                )
            })
            .collect();
        format!(
            "分段耗时（四段合计 {:.1} ms）：\n{}",
            total.as_secs_f64() * 1000.0,
            lines.join("\n")
        )
    }
}

/// 一次批扫的完整度量结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleMeasurement {
    /// 本档目标件数（200 / 500 / 1000）。
    pub target_items: usize,
    /// 实际进入规则层的件数（向上取整后 ≥ `target_items`）。
    pub items: usize,
    /// 住院（就诊组）数。
    pub encounters: usize,
    pub segments: SegmentProfile,
    /// 四段之外的墙钟耗时（生成夹具、断言准备等）。
    pub wall: Duration,
    /// 探针读到的进程峰值 RSS（字节）。
    pub peak_rss_bytes: u64,
    /// 命中诊断条数。
    pub findings: usize,
    /// 其中阻断级条数。
    pub blocking: usize,
    /// 增量命中率（跳过件数 / 进入规则层的件数）。
    pub hit_rate: f64,
}

impl ScaleMeasurement {
    /// 耗时取四段之和而非墙钟：墙钟里混着夹具生成与断言准备，
    /// 拿它当「审查耗时」报出去会高估产品自己的开销。
    pub fn measured(&self) -> Duration {
        self.segments.total()
    }

    /// 峰值 RSS 的人类可读值（MiB，保留一位）。
    pub fn peak_rss_mib(&self) -> f64 {
        self.peak_rss_bytes as f64 / (1024.0 * 1024.0)
    }

    /// profile 摘要（`docs/` 结果文件与 CLI 共用）。
    pub fn as_zh(&self) -> String {
        format!(
            "目标 {} 件 · 实际 {} 件 · 住院 {} 组 · 命中 {} 条（阻断 {}）· 增量命中率 {:.1}%\n\
             墙钟 {:.1} ms（其中四段合计 {:.1} ms）· 峰值内存 {:.1} MiB\n{}",
            self.target_items,
            self.items,
            self.encounters,
            self.findings,
            self.blocking,
            self.hit_rate * 100.0,
            self.wall.as_secs_f64() * 1000.0,
            self.measured().as_secs_f64() * 1000.0,
            self.peak_rss_mib(),
            self.segments.as_zh()
        )
    }
}

/// 计时器：包住一段主路径工作，把耗时累加进 [`SegmentProfile`]。
///
/// 用法是 `let _t = StageTimer::new(Stage::Parse, &mut profile);`，
/// 靠 `Drop` 封口——中途 `?` 提前返回也不会漏记（漏记会让四段之和
/// 与总耗时对不上，而对不上正是 [`SegmentProfile::is_balanced`] 要抓的）。
pub struct StageTimer<'a> {
    stage: Stage,
    profile: &'a mut SegmentProfile,
    started: Instant,
}

impl<'a> StageTimer<'a> {
    pub fn new(stage: Stage, profile: &'a mut SegmentProfile) -> Self {
        Self {
            stage,
            profile,
            started: Instant::now(),
        }
    }
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.profile.add(self.stage, self.started.elapsed());
    }
}

impl SegmentProfile {
    /// 四段之和是否与外部记的墙钟对得上（允许 1 ms 抖动）。
    ///
    /// 「对不上」意味着有工作发生在这四段之外却没被计时——
    /// 那部分的耗时会被漏报，一份漏了开销的 profile 比没有更危险。
    pub fn is_balanced_within(&self, wall: Duration, slack_ms: u64) -> bool {
        let slack = Duration::from_millis(slack_ms);
        self.total() <= wall + slack
    }
}

/// 读一次进程峰值 RSS（字节）。读不到时返回 `None` 而非 0。
///
/// 返回 `None` 而不是 0：0 会被上界断言当成「内存占用极低」直接放过，
/// 而真实情况是**探针没读到**。基准报告里出现 `None` 时应当由调用方
/// 显式决定是放行还是失败，不能悄悄当成「内存没问题」。
pub fn peak_rss_bytes() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `getrusage` 只向传入的 `rusage` 写本进程的资源用量；
    // `usage` 是本栈帧上的、可写的零初始化结构体，长度与 libc 声明一致。
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    let raw = usage.ru_maxrss as u64;
    if raw == 0 {
        return None;
    }
    Some(if cfg!(target_os = "macos") {
        // macOS：字节。
        raw
    } else {
        // Linux 等：KiB。
        raw * 1024
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_are_stable_and_distinct() {
        // 这四个名字会进 profile 结果文件与文档，跨机器对不上就没法比。
        let names: Vec<&str> = Stage::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["parse", "review", "graph", "store"]);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "段名不得重复");
        for s in Stage::ALL {
            assert!(!s.as_zh().is_empty());
        }
    }

    #[test]
    fn segments_accumulate_into_the_right_slot() {
        let mut p = SegmentProfile::empty();
        p.add(Stage::Parse, Duration::from_millis(10));
        p.add(Stage::Parse, Duration::from_millis(5));
        p.add(Stage::Review, Duration::from_millis(20));
        assert_eq!(p.get(Stage::Parse), Duration::from_millis(15));
        assert_eq!(p.get(Stage::Review), Duration::from_millis(20));
        assert_eq!(p.get(Stage::Graph), Duration::ZERO);
        assert_eq!(p.total(), Duration::from_millis(35));
    }

    #[test]
    fn shares_sum_to_one_hundred_percent() {
        let mut p = SegmentProfile::empty();
        p.add(Stage::Parse, Duration::from_millis(10));
        p.add(Stage::Review, Duration::from_millis(20));
        p.add(Stage::Graph, Duration::from_millis(30));
        p.add(Stage::Store, Duration::from_millis(40));
        let sum: f64 = Stage::ALL.iter().map(|s| p.share_of(*s)).sum();
        assert!((sum - 100.0).abs() < 1e-9, "占比之和应等于 100%，实为 {sum}");
        assert_eq!(p.slowest(), Some(Stage::Store));
    }

    #[test]
    fn an_all_zero_profile_has_no_slowest_stage() {
        // 四段全 0 时硬指一个「最慢」段会凭空造出一条现场追问不出来的问题。
        let p = SegmentProfile::empty();
        assert_eq!(p.slowest(), None);
        assert_eq!(p.share_of(Stage::Parse), 0.0);
    }

    #[test]
    fn the_stage_timer_records_on_drop_even_when_the_scope_returns_early() {
        let mut p = SegmentProfile::empty();
        {
            let _t = StageTimer::new(Stage::Graph, &mut p);
            std::thread::sleep(Duration::from_millis(12));
        }
        assert!(
            p.get(Stage::Graph) >= Duration::from_millis(10),
            "计时器应至少记到 sleep 的时长，实为 {:?}",
            p.get(Stage::Graph)
        );
    }

    #[test]
    fn four_segments_never_exceed_the_wall_clock_they_were_measured_inside() {
        // 四段是**串行**执行的，之和不可能超过包住它们的墙钟。
        // 对不上说明有段被重复累加，或计时器在段外还活着。
        //
        // 串行由借用检查器保证：`StageTimer` 独占 `&mut SegmentProfile`，
        // 因此两个计时器无法同时活着，嵌套计时在编译期就写不出来。
        let mut p = SegmentProfile::empty();
        let wall_start = Instant::now();
        {
            let _a = StageTimer::new(Stage::Parse, &mut p);
            std::thread::sleep(Duration::from_millis(3));
        }
        {
            let _b = StageTimer::new(Stage::Review, &mut p);
            std::thread::sleep(Duration::from_millis(3));
        }
        let wall = wall_start.elapsed();
        assert!(
            p.is_balanced_within(wall, 0),
            "四段之和 {:?} 不应超过墙钟 {:?}",
            p.total(),
            wall
        );
        assert_eq!(p.get(Stage::Parse) + p.get(Stage::Review), p.total());
    }

    #[test]
    fn peak_rss_is_readable_and_plausible() {
        let bytes = peak_rss_bytes().expect("探针应能读到本进程峰值 RSS");
        // 一个 Rust 测试进程的 RSS 至少几 MiB；读到 0 或个位数说明单位换算错了。
        assert!(
            bytes >= 4 * 1024 * 1024,
            "峰值 RSS 读数 {:.1} MiB 小得不合理，单位换算多半错了",
            bytes as f64 / (1024.0 * 1024.0)
        );
        assert!(
            bytes < 64 * 1024 * 1024 * 1024,
            "峰值 RSS 读数 {:.1} GiB 大得不合理，单位换算多半错了",
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        );
    }

    #[test]
    fn the_measurement_summary_names_every_stage() {
        let mut p = SegmentProfile::empty();
        p.add(Stage::Parse, Duration::from_millis(4));
        p.add(Stage::Review, Duration::from_millis(4));
        let m = ScaleMeasurement {
            target_items: 200,
            items: 204,
            encounters: 34,
            segments: p,
            wall: Duration::from_millis(9),
            peak_rss_bytes: 12 * 1024 * 1024,
            findings: 0,
            blocking: 0,
            hit_rate: 1.0,
        };
        let text = m.as_zh();
        for stage in Stage::ALL {
            assert!(text.contains(stage.as_zh()), "摘要应含段落「{}」", stage.as_zh());
        }
        assert!(text.contains("204 件"), "摘要应含实际件数");
        assert!((m.peak_rss_mib() - 12.0).abs() < 1e-9);
        assert_eq!(m.measured(), Duration::from_millis(8));
    }
}
