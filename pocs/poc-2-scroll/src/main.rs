//! POC-2：gix 大仓库虚拟化滚动帧时间。
//!
//! 对应实施计划 M0 节 POC-2 与需求 NFR-PRF-02 / NFR-MEM-03：
//!   - 百万级提交列表滚动，P95 帧时间 < 33ms；
//!   - 滚动过程中内存无持续增长。
//!
//! 实现要点（对齐架构规划 5.2 / 5.8）：
//!   - gix revwalk 一次性枚举提交 id（分页加载的等价成本计入"加载耗时"）；
//!   - ListView 只向模型索取可见行，模型按需 `find_commit` 解析并缓存
//!     （字符串驻留 + 惰性解析的等价验证）；
//!   - 由 1ms 定时器连续推进滚动偏移，采样相邻帧间隔作为帧耗时。
//!
//! 用法：`poc2-scroll <仓库路径> [--hold SECS]`

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, ModelTracker, SharedString};

slint::include_modules!();

/// 行高（与 .slint 中的 height 保持一致）。
const ROW_H: f32 = 24.0;

/// 每帧推进的行数。取较大步长以便在有限采样时长内遍历整个百万级列表，
/// 使帧耗时样本覆盖 packfile 的不同区段，而非只停留在列表头部。
const SCROLL_STRIDE_ROWS: usize = 4096;

/// 行文本缓存上限。超出即整体清空 —— 用于验证"内存不随访问行数线性增长"，
/// 生产实现会替换为 LRU。
const CACHE_CAP: usize = 8192;

fn bs(b: &gix::bstr::BStr) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn int_arg(flag: &str) -> Option<u64> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = (((values.len() as f64) - 1.0) * q).round() as usize;
    values[idx.min(values.len() - 1)]
}

/// 提交行模型：持有提交 id 列表，按可见性惰性解析提交信息。
struct CommitRows {
    repo: gix::Repository,
    ids: Vec<gix::ObjectId>,
    cache: RefCell<HashMap<usize, SharedString>>,
    /// 与主线程共享的统计（调用次数与累计耗时）。
    calls: Rc<Cell<u64>>,
    spent: Rc<RefCell<Duration>>,
}

impl Model for CommitRows {
    type Data = SharedString;

    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn row_data(&self, row: usize) -> Option<SharedString> {
        let id = *self.ids.get(row)?;
        if let Some(v) = self.cache.borrow().get(&row) {
            return Some(v.clone());
        }

        let began = Instant::now();
        let text = match self.repo.find_commit(id) {
            Ok(commit) => {
                let who = commit.author().map(|a| bs(a.name)).unwrap_or_default();
                let full = id.to_string();
                let short = &full[..8.min(full.len())];
                format!("{short}  {who:<12}  #{row}")
            }
            Err(_) => format!("{id}  <解析失败>"),
        };
        *self.spent.borrow_mut() += began.elapsed();
        self.calls.set(self.calls.get() + 1);

        let value: SharedString = text.into();
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
        cache.insert(row, value.clone());
        Some(value)
    }

    fn set_row_data(&self, _row: usize, _data: SharedString) {}

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

/// 泳道占位列：等宽文本模拟分支图，独立于提交数据（避免额外 I/O）。
struct LaneStub {
    count: usize,
}

impl Model for LaneStub {
    type Data = SharedString;

    fn row_count(&self) -> usize {
        self.count
    }

    fn row_data(&self, row: usize) -> Option<SharedString> {
        if row >= self.count {
            return None;
        }
        const WIDTH: usize = 6;
        let lane = row % WIDTH;
        let mut s = String::with_capacity(WIDTH * 2);
        for c in 0..WIDTH {
            s.push(if c == lane { '●' } else { '│' });
            s.push(' ');
        }
        Some(s.into())
    }

    fn set_row_data(&self, _row: usize, _data: SharedString) {}

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

fn main() {
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }

    let repo_path = std::env::args()
        .nth(1)
        .filter(|a| !a.starts_with("--"))
        .unwrap_or_else(|| {
            let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-repos/repos/large");
            p.to_string_lossy().into_owned()
        });
    let hold = int_arg("--hold").unwrap_or(30);
    let backend = std::env::var("SLINT_BACKEND").unwrap_or_default();

    println!("POC-2 虚拟化滚动：仓库 = {repo_path}  hold = {hold}s  backend = {backend}");

    // ---------- 加载：revwalk 枚举全部提交（分页加载成本的等价度量） ----------
    let t = Instant::now();
    let repo = match gix::open(&repo_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("打开仓库失败: {e}");
            return;
        }
    };

    let platform = match repo.references() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("读取引用失败: {e}");
            return;
        }
    };
    let iter = match platform.all() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("引用迭代失败: {e}");
            return;
        }
    };
    let mut tips = Vec::new();
    for r in iter.flatten() {
        tips.push(r.id().detach());
    }

    let mut ids = Vec::new();
    match repo.rev_walk(tips).all() {
        Ok(walk) => {
            for info in walk.flatten() {
                ids.push(info.id);
            }
        }
        Err(e) => {
            eprintln!("rev_walk 失败: {e}");
            return;
        }
    }
    let load = t.elapsed();
    println!(
        "加载完成：{} 个提交，耗时 {:?}（约 {:.2} 万条/秒）",
        ids.len(),
        load,
        ids.len() as f64 / 10_000.0 / load.as_secs_f64()
    );

    let row_count = ids.len();
    let max_offset = (row_count as f32 * ROW_H) - 640.0;

    // 与模型共享的行解析统计。
    let calls = Rc::new(Cell::new(0u64));
    let spent = Rc::new(RefCell::new(Duration::ZERO));

    // ---------- 建窗与模型 ----------
    let win = Poc2Window::new().expect("创建窗口失败");
    win.set_heading("POC-2 · gix 虚拟化提交列表".into());
    win.set_info(format!("{row_count} 行 · 后端 {backend} · 连续滚动采样").into());
    win.set_rows(ModelRc::new(CommitRows {
        repo,
        ids,
        cache: RefCell::new(HashMap::new()),
        calls: calls.clone(),
        spent: spent.clone(),
    }));
    win.set_graph(ModelRc::new(LaneStub { count: row_count }));
    win.show().expect("显示窗口失败");

    // ---------- 帧耗时采样 + 连续滚动驱动 ----------
    let frames: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
    let last = Rc::new(Cell::new(Instant::now()));
    let offset = Rc::new(Cell::new(0.0f32));

    // 优先尝试渲染回调（若后端支持），失败则忽略。
    {
        let frames = frames.clone();
        let last = last.clone();
        let notifier = win.window().set_rendering_notifier(move |state, _api| {
            if let slint::RenderingState::BeforeRendering = state {
                let now = Instant::now();
                let dt = now.duration_since(last.get()).as_secs_f64() * 1000.0;
                if dt > 0.0 {
                    frames.borrow_mut().push(dt);
                }
                last.set(now);
            }
        });
        if notifier.is_err() {
            println!("提示：当前后端不支持渲染回调，改用定时器采样帧间隔。");
        }
    }

    let weak = win.as_weak();
    let frames_timer = frames.clone();
    let last_timer = last.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1),
        move || {
            let now = Instant::now();
            let dt = now.duration_since(last_timer.get()).as_secs_f64() * 1000.0;
            frames_timer.borrow_mut().push(dt);
            last_timer.set(now);

            let mut o = offset.get() + ROW_H * SCROLL_STRIDE_ROWS as f32;
            if o > max_offset || o < 0.0 {
                o = 0.0;
            }
            offset.set(o);
            if let Some(w) = weak.upgrade() {
                w.set_scroll_y(-o);
            }
        },
    );

    let _quit = slint::Timer::single_shot(Duration::from_secs(hold), || {
        let _ = slint::quit_event_loop();
    });

    slint::run_event_loop().expect("事件循环异常");

    // ---------- 报告 ----------
    let mut data = frames.borrow().clone();
    let total = data.len();
    if total == 0 {
        println!("未采集到任何帧数据（窗口可能被立即关闭）。");
        return;
    }
    let slow = data.iter().filter(|v| **v > 33.0).count();
    let p50 = percentile(&mut data, 0.50);
    let p95 = percentile(&mut data, 0.95);
    let p99 = percentile(&mut data, 0.99);
    let max = data.iter().cloned().fold(0.0f64, f64::max);
    println!("\n=== POC-2 结果 ===");
    println!("采样帧数：{total}");
    println!("帧间隔 P50 = {p50:.2} ms   P95 = {p95:.2} ms   P99 = {p99:.2} ms   最大 = {max:.2} ms");
    println!(
        "超过 33ms 的帧：{slow} 帧（{:.3}%）  判据 P95<33ms：{}",
        slow as f64 * 100.0 / total as f64,
        if p95 < 33.0 { "通过" } else { "未通过" }
    );
    let call_count = calls.get();
    let total_spent = spent.borrow().as_secs_f64() * 1000.0;
    println!(
        "行解析：{call_count} 次，累计 {total_spent:.1} ms，平均 {:.3} ms/行（缓存上限 {CACHE_CAP}）",
        if call_count == 0 {
            0.0
        } else {
            total_spent / call_count as f64
        }
    );
}
