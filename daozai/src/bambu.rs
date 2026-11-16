//! 打印页：本机「拓竹打印监控」（BambuMonitor）的实时打印进度。
//!
//! 岛仔自己不连接打印机、也不接触打印机凭据：数据来自 BambuMonitor 提供的
//! **只读本地状态桥**（`GET http://127.0.0.1:<port>/status`，见该项目的
//! `bambu_monitor/bridge.py`）。桥没启动或监控退出时，本页显示「未连接」，
//! 不会影响其它功能，也不会重试到刷屏。
//!
//! 这一整个功能（取数 + 渲染）都收在这个文件里，页面注册只有几行机械改动；
//! 以后岛仔的插件系统开放「贡献一个页面」的能力时，可以整块搬走。
//!
//! # 数据契约
//!
//! ```json
//! { "api": 1, "source": "bambumon", "updated": 1763300000.0, "age": 1.2,
//!   "printer": {"name": "Good Mini", "model": "A1 mini"},
//!   "status": { "online": true, "gcode_state": "RUNNING", "state_text": "打印中",
//!               "stage_text": "打印中", "percent": 37, "remaining": 42,
//!               "layer": 118, "total_layer": 320, "file": "benchy.3mf",
//!               "nozzle": 214.5, "nozzle_target": 215.0,
//!               "bed": 59.8, "bed_target": 60.0, "hms": [], "ams_text": "空闲" } }
//! ```

use crate::app::App;
use crate::paint::{self, capsule, Card, H};
use crate::theme::{self, size, W};
use eframe::egui;
use eframe::egui::Color32;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

/// 桥的默认端口，与 BambuMonitor 的 `bridge.py` 保持一致。
pub const DEFAULT_PORT: u16 = 39778;
/// 端口被占用时监控会往后试几个，实际端口写在 bridge.json 里。
const PORT_FILE: &str = "BambuMonitor\\rt\\bridge.json";
/// 页面在前台时的轮询间隔（秒）。
const POLL_VISIBLE: f64 = 1.0;
/// 页面不在前台时的轮询间隔；只为让收起的状态保持新鲜。
const POLL_IDLE: f64 = 5.0;
/// 桥的快照超过这么多秒没更新，就当监控掉线了。
const STALE_AFTER: f64 = 20.0;
/// 连续失败后的最大退避（秒）：监控没开时不至于每秒敲一次端口。
const BACKOFF_MAX: f64 = 30.0;

// 版式（面板局部逻辑点）：左边进度环，右边五行文字，底下一行动作。
const RING_X: f32 = 66.0;
const RING_Y: f32 = 78.0;
const RING_R: f32 = 28.0;
const RING_W: f32 = 7.0;
/// 右栏起点与可用宽度（面板宽 419，右侧留 15 的边）。
const COL_X: f32 = 106.0;
const COL_W: f32 = 298.0;

#[derive(Clone, Debug, Default)]
pub struct Printer {
    pub name: String,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub online: bool,
    /// `RUNNING` / `PAUSE` / `FINISH` / `FAILED` / `IDLE` …
    pub gcode: String,
    /// 监控算好的中文状态（打印中 / 已暂停 …）。
    pub state_text: String,
    /// 当前阶段（热床预热、换料中 …）。
    pub stage_text: String,
    pub percent: f64,
    /// 剩余分钟数。
    pub remaining: Option<f64>,
    pub layer: Option<f64>,
    pub total_layer: Option<f64>,
    pub file: String,
    pub nozzle: Option<f64>,
    pub nozzle_target: Option<f64>,
    pub bed: Option<f64>,
    pub bed_target: Option<f64>,
    pub hms: usize,
    pub print_error: f64,
    pub ams_text: String,
}

impl Status {
    /// 正在打印（含准备阶段）。
    pub fn busy(&self) -> bool {
        matches!(self.gcode.as_str(), "RUNNING" | "PREPARE" | "SLICING")
    }

    pub fn paused(&self) -> bool {
        self.gcode == "PAUSE"
    }

    pub fn failed(&self) -> bool {
        self.gcode == "FAILED" || self.print_error > 0.0
    }

    /// 状态文案：优先用监控翻译好的中文。
    pub fn label(&self) -> &str {
        if !self.state_text.trim().is_empty() {
            return self.state_text.trim();
        }
        match self.gcode.as_str() {
            "RUNNING" => "打印中",
            "PREPARE" => "准备中",
            "SLICING" => "切片中",
            "PAUSE" => "已暂停",
            "FINISH" => "已完成",
            "FAILED" => "失败",
            "IDLE" => "空闲",
            "OFFLINE" => "离线",
            _ => "未知",
        }
    }

    /// 进度环的渐变（上亮下暗），按状态换色。
    pub fn accent(&self) -> (Color32, Color32) {
        if self.failed() {
            (theme::RED_TOP, theme::RED)
        } else if self.paused() {
            (Color32::from_rgb(255, 214, 92), Color32::from_rgb(206, 150, 20))
        } else if self.gcode == "FINISH" {
            (Color32::from_rgb(140, 205, 255), theme::BLUE_DEEP)
        } else if self.busy() {
            (Color32::from_rgb(112, 232, 160), Color32::from_rgb(30, 158, 92))
        } else {
            (Color32::from_gray(168), Color32::from_gray(116))
        }
    }

    /// 剩余时间文案：`1 小时 20 分` / `42 分钟`。
    pub fn remaining_text(&self) -> String {
        let Some(minutes) = self.remaining else { return String::new(); };
        let m = minutes.round() as i64;
        if m <= 0 {
            return String::new();
        }
        let (h, mm) = (m / 60, m % 60);
        if h > 0 && mm > 0 {
            format!("{h} 小时 {mm} 分")
        } else if h > 0 {
            format!("{h} 小时")
        } else {
            format!("{mm} 分钟")
        }
    }

    pub fn layer_text(&self) -> String {
        match (self.layer, self.total_layer) {
            (Some(layer), Some(total)) if total > 0.0 => {
                format!("{} / {} 层", layer.round() as i64, total.round() as i64)
            }
            (Some(layer), _) => format!("{} 层", layer.round() as i64),
            _ => String::new(),
        }
    }
}

/// 一次成功拉取的完整快照。
#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub printer: Printer,
    pub status: Status,
    /// 桥自己算的「该快照距今多少秒」。
    pub age: f64,
}

impl Doc {
    /// 数据还新鲜、并且打印机在线，才按「有进度」显示。
    pub fn live(&self) -> bool {
        self.age < STALE_AFTER && self.status.online
    }
}

// ---------------------------------------------------------------------------
// 解析：字段类型会随固件/版本变（数字有时是字符串），所以逐个宽松取值。
// ---------------------------------------------------------------------------
fn text(value: &serde_json::Value, key: &str) -> String {
    match value.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn num(value: &serde_json::Value, key: &str) -> Option<f64> {
    match value.get(key)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')).collect();
            cleaned.parse().ok()
        }
        _ => None,
    }
}

fn flag(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// 把桥的响应解析成快照。格式不对时返回错误，界面据此显示「未连接」。
pub fn parse(body: &str) -> Result<Doc, String> {
    let root: serde_json::Value = serde_json::from_str(body).map_err(|_| "状态桥返回的不是 JSON".to_string())?;
    if !root.is_object() {
        return Err("状态桥返回的 JSON 不是对象".into());
    }
    let api = num(&root, "api").unwrap_or(0.0);
    if api != 1.0 {
        return Err(format!("状态桥接口版本不符（{api}），请升级打印监控"));
    }
    let printer_value = root.get("printer").cloned().unwrap_or(serde_json::Value::Null);
    let status_value = root.get("status").cloned().unwrap_or(serde_json::Value::Null);
    let hms = status_value.get("hms").and_then(|v| v.as_array()).map(|list| list.len()).unwrap_or(0);
    Ok(Doc {
        printer: Printer { name: text(&printer_value, "name"), model: text(&printer_value, "model") },
        status: Status {
            online: flag(&status_value, "online"),
            gcode: text(&status_value, "gcode_state").to_uppercase(),
            state_text: text(&status_value, "state_text"),
            stage_text: text(&status_value, "stage_text"),
            percent: num(&status_value, "percent").unwrap_or(0.0).clamp(0.0, 100.0),
            remaining: num(&status_value, "remaining"),
            layer: num(&status_value, "layer"),
            total_layer: num(&status_value, "total_layer"),
            file: text(&status_value, "file"),
            nozzle: num(&status_value, "nozzle"),
            nozzle_target: num(&status_value, "nozzle_target"),
            bed: num(&status_value, "bed"),
            bed_target: num(&status_value, "bed_target"),
            hms,
            print_error: num(&status_value, "print_error").unwrap_or(0.0),
            ams_text: text(&status_value, "ams_text"),
        },
        age: num(&root, "age").unwrap_or(f64::MAX),
    })
}

// ---------------------------------------------------------------------------
// 取数
// ---------------------------------------------------------------------------
/// 读取桥的实际端口：监控把端口写在 `%APPDATA%\BambuMonitor\rt\bridge.json`，
/// 读不到就用默认端口。`DAOZAI_BAMBU_PORT` 仅供调试指定。
pub fn discover_port() -> u16 {
    if let Ok(value) = std::env::var("DAOZAI_BAMBU_PORT") {
        if let Ok(port) = value.trim().parse::<u16>() {
            return port;
        }
    }
    let Some(appdata) = std::env::var_os("APPDATA") else { return DEFAULT_PORT; };
    let path = std::path::Path::new(&appdata).join(PORT_FILE);
    let Ok(raw) = std::fs::read_to_string(path) else { return DEFAULT_PORT; };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { return DEFAULT_PORT; };
    value.get("port").and_then(serde_json::Value::as_u64).filter(|p| *p > 0 && *p < 65536).map(|p| p as u16).unwrap_or(DEFAULT_PORT)
}

/// 只连本机回环地址：显式忽略环境里的代理，否则 `127.0.0.1` 会被送进代理。
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().try_proxy_from_env(false)
        .timeout_connect(Duration::from_millis(700))
        .timeout(Duration::from_millis(1500))
        .build()
}

fn fetch(port: u16) -> Result<Doc, String> {
    let url = format!("http://127.0.0.1:{port}/status");
    let response = http_agent().get(&url).call().map_err(|error| match error {
        // 监控没运行时是最常见的「连接被拒绝」，文案直接指向该做的事。
        ureq::Error::Transport(_) => "没有连上打印监控".to_string(),
        _ => "读取打印状态失败".to_string(),
    })?;
    if response.status() != 200 {
        return Err(format!("状态桥返回 {}", response.status()));
    }
    let body = response.into_string().map_err(|_| "读取打印状态失败".to_string())?;
    parse(&body)
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------
pub struct State {
    pub snapshot: Option<Doc>,
    /// 最近一次失败的原因；有快照时只作为脚注，不抢主显示。
    pub error: Option<String>,
    pub loading: bool,
    /// 进度环的缓动量（0..1），让百分比变化有「走」的感觉。
    pub shown: crate::anim::Spring,
    last_start: f64,
    failures: u32,
    port: u16,
    /// 端口只查一次；之后只有失败时才重新读监控写的 bridge.json。
    discovered: bool,
    rx: Option<Receiver<Result<Doc, String>>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            snapshot: None,
            error: None,
            loading: false,
            shown: crate::anim::Spring::new(0.0, 120.0, 0.9),
            last_start: -100.0,
            failures: 0,
            port: DEFAULT_PORT,
            discovered: false,
            rx: None,
        }
    }
}

impl State {
    /// 失败次数越多退避越久；一次成功就清零。
    fn backoff(&self) -> f64 {
        if self.failures == 0 {
            0.0
        } else {
            (POLL_VISIBLE * 2f64.powi(self.failures.min(5) as i32)).min(BACKOFF_MAX)
        }
    }

    pub fn interval(&self, visible: bool) -> f64 {
        let base = if visible { POLL_VISIBLE } else { POLL_IDLE };
        base.max(self.backoff())
    }

    pub fn should_refresh(&self, now: f64, visible: bool) -> bool {
        !self.loading && now - self.last_start >= self.interval(visible)
    }

    /// 起一次后台请求（不阻塞界面）。
    pub fn refresh(&mut self, now: f64) {
        // 端口只在第一次（以及每次失败后）去读监控写的 bridge.json，不必每秒读文件。
        let port = if self.discovered && self.failures == 0 { self.port } else { discover_port() };
        self.port = port;
        self.discovered = true;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.loading = true;
        self.last_start = now;
        std::thread::spawn(move || {
            let _ = tx.send(fetch(port));
        });
    }

    /// 收集后台结果；有新数据时返回 true（外壳据此立即重绘）。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false; };
        match rx.try_recv() {
            Ok(result) => {
                self.rx = None;
                self.loading = false;
                match result {
                    Ok(doc) => {
                        self.snapshot = Some(doc);
                        self.error = None;
                        self.failures = 0;
                    }
                    Err(error) => {
                        self.error = Some(error);
                        self.failures = self.failures.saturating_add(1);
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.loading = false;
                self.error = Some("读取打印状态失败".into());
                self.failures = self.failures.saturating_add(1);
                true
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

// ---------------------------------------------------------------------------
// 页面
// ---------------------------------------------------------------------------
/// 进度环：从 12 点方向顺时针铺，上亮下暗。
fn progress_ring(c: &Card, cx: f32, cy: f32, radius: f32, width: f32, frac: f32, top: Color32, bottom: Color32) {
    c.ring(cx, cy, radius, width, Color32::from_rgb(38, 38, 42));
    let frac = frac.clamp(0.0, 1.0);
    if frac <= 0.001 {
        return;
    }
    let steps = (120.0 * frac).ceil().max(2.0) as usize;
    let a0 = -FRAC_PI_2;
    let mut prev = (cx + radius * a0.cos(), cy + radius * a0.sin());
    for i in 1..=steps {
        let a = a0 + TAU * frac * i as f32 / steps as f32;
        let next = (cx + radius * a.cos(), cy + radius * a.sin());
        let t = ((prev.1 + next.1) * 0.5 - (cy - radius)) / (2.0 * radius);
        c.seg(prev, next, width, paint::mix(top, bottom, t));
        prev = next;
    }
    c.circle(cx, cy - radius, width * 0.5, top);
    c.circle(prev.0, prev.1, width * 0.5, paint::mix(top, bottom, ((prev.1 - (cy - radius)) / (2.0 * radius)).clamp(0.0, 1.0)));
}

impl App {
    /// 打印页。坐标是面板局部逻辑点（419×BAMBU_H），文字 y 是基线。
    pub(crate) fn page_bambu(&mut self, c: &Card) {
        match self.bambu.snapshot.clone() {
            Some(doc) if doc.live() => self.bambu_connected(c, &doc),
            // 桥还在、只是打印机离线（或者快照不再更新）：保留上次的画面，压暗并说明原因。
            Some(doc) => self.bambu_stale(c, &doc),
            None => self.bambu_offline(c),
        }
    }

    fn bambu_connected(&mut self, c: &Card, doc: &Doc) {
        let status = &doc.status;
        let (top, bottom) = status.accent();
        let (cx, cy, radius) = (RING_X, RING_Y, RING_R);
        c.glow(cx, cy, 42.0, paint::alpha(bottom, 0.30));
        progress_ring(c, cx, cy, radius, RING_W, self.bambu.shown.pos, top, bottom);

        // 环心：百分比。数字与「%」分开排，小字号里才不会挤成一团。
        let pct = format!("{:.0}", status.percent);
        let number_w = c.text_w(&pct, 20.0, W::Bold);
        let total_w = number_w + c.text_w("%", 10.0, W::Semibold) + 0.5;
        c.centered_text(cx - total_w / 2.0 + number_w / 2.0, cy, &pct, 20.0, W::Bold, theme::WHITE, H::Center);
        c.text(cx - total_w / 2.0 + number_w + 0.5, cy + 3.5, "%", 10.0, W::Semibold, paint::mix(theme::WHITE, top, 0.5), H::Left);

        // 右侧五行：文件名 / 状态·阶段 / 层数与剩余 / 温度 / 告警或 AMS
        let left = COL_X;
        let name = if status.file.trim().is_empty() { "（未在打印）".to_string() } else { status.file.clone() };
        let name = c.fit(&name, size::BODY, W::Semibold, COL_W);
        c.text(left, 48.0, &name, size::BODY, W::Semibold, theme::WHITE, H::Left);

        let mut x = left;
        if status.failed() || status.paused() {
            c.circle(left + 3.0, 59.5, 3.0, paint::mix(top, theme::WHITE, 0.2));
            x = left + 11.0;
        }
        x += c.text(x, 63.0, status.label(), size::SMALL, W::Semibold, top, H::Left);
        let stage = status.stage_text.trim();
        if !stage.is_empty() && stage != status.label() {
            x += c.text(x, 63.0, " · ", size::SMALL, W::Regular, theme::DIM, H::Left);
            c.text(x, 63.0, &c.fit(stage, size::SMALL, W::Regular, COL_W - (x - left)), size::SMALL, W::Regular, theme::GRAY, H::Left);
        }

        let mut line = status.layer_text();
        let remain = status.remaining_text();
        if !remain.is_empty() {
            if !line.is_empty() {
                line.push_str(" · ");
            }
            line.push_str("还剩 ");
            line.push_str(&remain);
        }
        if !line.is_empty() {
            c.text(left, 78.0, &line, size::SMALL, W::Regular, theme::GRAY, H::Left);
        }

        // 温度：当前值白色，目标值压暗
        let mut tx = left;
        let mut any_temp = false;
        if let Some(nozzle) = status.nozzle {
            tx += c.text(tx, 93.0, "喷嘴 ", size::TINY, W::Regular, theme::DIM, H::Left);
            tx += c.text(tx, 93.0, &format!("{nozzle:.0}"), size::SMALL, W::Semibold, theme::WHITE, H::Left);
            if let Some(target) = status.nozzle_target {
                tx += c.text(tx, 93.0, &format!("/{target:.0}°"), size::TINY, W::Regular, theme::DIM, H::Left);
            }
            any_temp = true;
        }
        if let Some(bed) = status.bed {
            tx += c.text(tx, 93.0, if any_temp { "   热床 " } else { "热床 " }, size::TINY, W::Regular, theme::DIM, H::Left);
            tx += c.text(tx, 93.0, &format!("{bed:.0}"), size::SMALL, W::Semibold, theme::WHITE, H::Left);
            if let Some(target) = status.bed_target {
                c.text(tx, 93.0, &format!("/{target:.0}°"), size::TINY, W::Regular, theme::DIM, H::Left);
            }
        }

        // 告警优先于 AMS：告警要人处理，换料只是过程。
        if status.hms > 0 {
            let color = if status.failed() { theme::RED_TOP } else { theme::YELLOW };
            c.text(left, 108.0, &format!("打印机告警 {} 项 · 请在屏幕上确认", status.hms), size::SMALL, W::Semibold, color, H::Left);
        } else if !status.ams_text.trim().is_empty() && status.ams_text.trim() != "空闲" {
            c.text(left, 108.0, &format!("AMS · {}", c.fit(status.ams_text.trim(), size::SMALL, W::Regular, COL_W - 30.0)), size::SMALL, W::Regular, theme::DIM, H::Left);
        }

        // 右上角：机型（很小，不抢内容）
        let mut meta = doc.printer.model.clone();
        if meta.is_empty() {
            meta = doc.printer.name.clone();
        }
        if !meta.is_empty() {
            let meta = c.fit(&meta, size::TINY, W::Regular, 120.0);
            c.text(374.0, 25.0, &meta, size::TINY, W::Regular, paint::alpha(theme::GRAY, 0.65), H::Right);
        }

        self.bambu_footer(c, doc.age, "刷新", "bambu-refresh");
    }

    /// 桥还在，但打印机离线（或快照停止更新）：保留上次画面，整体压暗并说明原因，
    /// 免得让人以为监控挂了。
    fn bambu_stale(&mut self, c: &Card, doc: &Doc) {
        let status = &doc.status;
        let dim = paint::alpha(theme::WHITE, 0.42);
        let (cx, cy, radius) = (RING_X, RING_Y, RING_R);
        progress_ring(c, cx, cy, radius, RING_W, self.bambu.shown.pos, Color32::from_gray(120), Color32::from_gray(70));
        let pct = format!("{:.0}", status.percent);
        let number_w = c.text_w(&pct, 20.0, W::Bold);
        let total_w = number_w + c.text_w("%", 10.0, W::Semibold) + 0.5;
        c.centered_text(cx - total_w / 2.0 + number_w / 2.0, cy, &pct, 20.0, W::Bold, dim, H::Center);
        c.text(cx - total_w / 2.0 + number_w + 0.5, cy + 3.5, "%", 10.0, W::Semibold, dim, H::Left);

        let left = COL_X;
        let name = if status.file.trim().is_empty() { "（不在打印）".to_string() } else { status.file.clone() };
        let name = c.fit(&name, size::BODY, W::Semibold, COL_W);
        c.text(left, 48.0, &name, size::BODY, W::Semibold, dim, H::Left);

        // 区分「打印机离线」和「监控没在刷新」：前者等打印机，后者要去看监控进程。
        // 只有正在打印/暂停/已完成/失败这类有信息量的状态才跟在后面，免得出现「离线 · 空闲」。
        let label = status.label();
        let informative = status.busy() || status.paused() || status.gcode == "FINISH" || status.failed();
        let reason = if doc.age >= STALE_AFTER {
            "监控已断开 · 显示的是上次状态".to_string()
        } else if informative {
            format!("打印机离线 · {label}")
        } else {
            "打印机离线".to_string()
        };
        c.text(left, 63.0, &reason, size::SMALL, W::Semibold, paint::alpha(theme::YELLOW, 0.75), H::Left);

        let mut line = status.layer_text();
        let remain = status.remaining_text();
        if !remain.is_empty() {
            if !line.is_empty() {
                line.push_str(" · ");
            }
            line.push_str("还剩 ");
            line.push_str(&remain);
        }
        if !line.is_empty() {
            c.text(left, 78.0, &line, size::SMALL, W::Regular, paint::alpha(theme::GRAY, 0.7), H::Left);
        }
        if status.hms > 0 {
            c.text(left, 93.0, &format!("上次有 {} 项告警", status.hms), size::SMALL, W::Regular, paint::alpha(theme::YELLOW, 0.8), H::Left);
        }

        self.bambu_footer(c, doc.age, "刷新", "bambu-refresh");
    }

    fn bambu_offline(&mut self, c: &Card) {
        let (cx, cy, radius) = (RING_X, RING_Y, RING_R);
        c.ring(cx, cy, radius, RING_W, Color32::from_rgb(38, 38, 42));
        c.centered_text(cx, cy, "--", 19.0, W::Bold, paint::alpha(theme::GRAY, 0.7), H::Center);

        let left = COL_X;
        c.text(left, 48.0, "未连接打印监控", size::BODY, W::Semibold, theme::GRAY, H::Left);
        c.text(left, 63.0, "启动 BambuMonitor 后自动接入", size::SMALL, W::Regular, theme::DIM, H::Left);
        c.text(left, 78.0, &format!("127.0.0.1:{}", self.bambu.port()), size::TINY, W::Regular, paint::alpha(theme::DIM, 0.85), H::Left);
        // 只有「不是没连上」的原因才单独写一行，避免和标题重复。
        if let Some(error) = &self.bambu.error {
            if error != "没有连上打印监控" {
                c.text(left, 93.0, &c.fit(error, size::TINY, W::Regular, COL_W), size::TINY, W::Regular, paint::alpha(theme::YELLOW, 0.85), H::Left);
            }
        }

        self.bambu_footer(c, -1.0, "重试", "bambu-retry");
    }

    /// 底部一行：左侧动作按钮，右侧状态说明。`age` 小于 0 表示没有数据。
    fn bambu_footer(&mut self, c: &Card, age: f64, action: &str, id: &str) {
        let (button, inner) = capsule(c, 15.0, 116.0, 62.0, 25.0, id, theme::BTN, theme::BTN_HOVER);
        paint::reset(&inner, 30.0, 128.5, 0.0, theme::WHITE);
        inner.text(41.0, 132.0, action, size::BODY, W::Regular, theme::WHITE, H::Left);
        if button.clicked() {
            self.bambu.refresh(self.t);
        }
        let note = if age < 0.0 {
            "在监控里开启状态桥即可接入".to_string()
        } else if age < 1.5 {
            "刚刚更新".to_string()
        } else {
            format!("{} 秒前更新", age.round() as i64)
        };
        c.text(90.0, 132.0, &note, size::TINY, W::Regular, theme::DIM, H::Left);
        if age >= 0.0 {
            if let Some(error) = &self.bambu.error {
                c.text(150.0, 132.0, &c.fit(error, size::TINY, W::Regular, 220.0), size::TINY, W::Regular, paint::alpha(theme::YELLOW, 0.8), H::Left);
            }
        }
    }
    /// 后台轮询：页面在前台时 1 秒一次，否则放慢；连续失败按退避拉长。
    pub(crate) fn bambu_tick(&mut self, dt: f32) {
        let visible = self.open && self.page == crate::app::BAMBU_PAGE;
        self.bambu.poll();
        // 目标每帧都跟着当前快照走：不管这一帧有没有新数据，环都对准真实进度。
        self.bambu.shown.target = match &self.bambu.snapshot {
            Some(doc) if doc.live() => doc.status.percent as f32 / 100.0,
            _ => 0.0,
        };
        self.bambu.shown.step(dt);
        if !self.module_on(crate::app::BAMBU_PAGE) {
            return;
        }
        if self.bambu.should_refresh(self.t, visible) {
            // 单元测试只验证调度，不真的去敲端口。
            if !cfg!(test) {
                self.bambu.refresh(self.t);
            } else {
                self.bambu.refresh_for_test(self.t);
            }
        }
    }
}

impl State {
    /// 测试用：只推进「上次请求时间」，不真的发请求。
    pub(crate) fn refresh_for_test(&mut self, now: f64) {
        self.last_start = now;
    }
}

impl App {
    /// 打印页的设置分页：这个功能本身没有可调项，主要是状态说明与桥的排查信息。
    pub(crate) fn bambu_settings(&mut self, ui: &mut egui::Ui) {
        use crate::extras::{settings_card, settings_label};
        ui.label(egui::RichText::new("打印进度来自本机的「拓竹打印监控」。岛仔不连接打印机，也不保存打印机凭据。").size(12.0).color(egui::Color32::from_gray(180)));
        settings_label(ui, "状态");
        settings_card(ui, |ui| {
            let live = self.bambu.snapshot.as_ref().filter(|doc| doc.live());
            match live {
                Some(doc) => {
                    ui.label(format!("已连接 · {} · {}", doc.printer.model, doc.status.label()));
                    if !doc.status.file.is_empty() {
                        ui.label(format!("当前任务：{}", doc.status.file));
                    }
                    let layer = doc.status.layer_text();
                    ui.label(if layer.is_empty() {
                        format!("进度 {:.0}%", doc.status.percent)
                    } else {
                        format!("进度 {:.0}% · {layer}", doc.status.percent)
                    });
                }
                None => {
                    ui.label("未连接");
                    if let Some(error) = &self.bambu.error {
                        ui.label(egui::RichText::new(error).size(11.5).color(egui::Color32::from_rgb(220, 170, 60)));
                    }
                }
            }
            ui.separator();
            ui.label(format!("状态桥地址：127.0.0.1:{}", self.bambu.port()));
            ui.horizontal(|ui| {
                if ui.button("立即刷新").clicked() {
                    self.bambu.refresh(self.t);
                }
            });
        });
        settings_label(ui, "监控端");
        settings_card(ui, |ui| {
            ui.label("在拓竹打印监控里保持运行即可；建议开启开机自启，并在其 config.json 中确认 app.bridge 为 true（默认开启）。");
            ui.label(egui::RichText::new("状态桥只监听本机回环地址、只有读取接口，不会上传任何数据。").size(11.5).color(egui::Color32::from_gray(150)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"{"api":1,"source":"bambumon","updated":1.0,"age":0.4,
            "printer":{"name":"Good Mini","model":"A1 mini"},
            "status":{"online":true,"gcode_state":"RUNNING","state_text":"打印中",
                      "stage_text":"打印中","percent":37,"remaining":42,
                      "layer":118,"total_layer":320,"file":"benchy.3mf",
                      "nozzle":214.5,"nozzle_target":215,"bed":59.8,"bed_target":60,
                      "hms":[],"ams_text":"空闲","print_error":0}}"#
    }

    #[test]
    fn parses_bridge_payload() {
        let doc = parse(sample()).expect("合法快照应能解析");
        assert_eq!(doc.printer.model, "A1 mini");
        assert_eq!(doc.status.gcode, "RUNNING");
        assert_eq!(doc.status.percent, 37.0);
        assert_eq!(doc.status.file, "benchy.3mf");
        assert_eq!(doc.status.layer_text(), "118 / 320 层");
        assert_eq!(doc.status.remaining_text(), "42 分钟");
        assert!(doc.live(), "在线且新鲜时算已连接");
    }

    #[test]
    fn rejects_wrong_api_version() {
        let body = sample().replace("\"api\":1", "\"api\":2");
        assert!(parse(&body).unwrap_err().contains("版本"));
    }

    #[test]
    fn tolerates_strings_and_missing_fields() {
        let body = r#"{"api":1,"printer":{},"status":{"online":true,"gcode_state":"pause","percent":"45","remaining":90,"nozzle":"210"}}"#;
        let doc = parse(body).expect("字段缺失或数字为字符串也要能读");
        assert_eq!(doc.status.gcode, "PAUSE", "状态统一转大写");
        assert_eq!(doc.status.percent, 45.0);
        assert_eq!(doc.status.remaining_text(), "1 小时 30 分");
        assert!(doc.status.paused());
    }

    #[test]
    fn stale_or_offline_is_not_live() {
        let mut doc = parse(sample()).unwrap();
        doc.age = STALE_AFTER + 1.0;
        assert!(!doc.live(), "快照太旧要按未连接显示");
        doc.age = 0.0;
        doc.status.online = false;
        assert!(!doc.live(), "监控报告离线时也按未连接显示");
    }

    #[test]
    fn percent_is_clamped_and_state_colors_follow_gcode() {
        let body = sample().replace("\"percent\":37", "\"percent\":137");
        assert_eq!(parse(&body).unwrap().status.percent, 100.0);
        let mut doc = parse(sample()).unwrap();
        doc.status.gcode = "FAILED".into();
        assert!(doc.status.failed());
        doc.status.gcode = "PAUSE".into();
        assert!(doc.status.paused());
        assert_eq!(doc.status.label(), "打印中", "有中文状态时优先用监控给的文案");
    }

    #[test]
    fn backoff_grows_then_resets() {
        let mut state = State::default();
        assert_eq!(state.interval(true), POLL_VISIBLE);
        state.failures = 3;
        assert!(state.interval(true) > POLL_VISIBLE);
        assert!(state.interval(false) >= state.interval(true));
        state.failures = 0;
        assert_eq!(state.interval(false), POLL_IDLE);
    }

    /// 真的走一次 HTTP：起一个只回一帧的本地服务，确认取数与解析这段能通。
    #[test]
    fn fetch_reads_status_over_http() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("测试用本地端口");
        let port = listener.local_addr().unwrap().port();
        let body = sample().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 512];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let doc = fetch(port).expect("应能读到状态桥的快照");
        assert_eq!(doc.status.percent, 37.0);
        assert_eq!(doc.printer.model, "A1 mini");
        assert!(doc.live());
    }

    /// 监控没运行时是最常见的场景：错误要指向「没连上」，而不是内部细节。
    #[test]
    fn fetch_reports_missing_bridge() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("测试用本地端口");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = fetch(port).expect_err("端口没人监听时应报错");
        assert_eq!(error, "没有连上打印监控");
    }

    /// 对着**本机真实运行**的状态桥读一次，用来验收整条链路：
    /// `DAOZAI_BAMBU_PORT=39778 cargo test reads_the_live_bridge -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机运行中的打印监控状态桥"]
    fn reads_the_live_bridge() {
        let Some(port) = std::env::var("DAOZAI_BAMBU_PORT").ok().and_then(|value| value.trim().parse::<u16>().ok()) else {
            return;
        };
        let doc = fetch(port).expect("本机状态桥应可读取");
        println!("live bridge: model={:?} name={:?} online={} state={} pct={:.0} file={:?} layer={} hms={}",
            doc.printer.model, doc.printer.name, doc.status.online, doc.status.label(),
            doc.status.percent, doc.status.file, doc.status.layer_text(), doc.status.hms);
        assert!(doc.live(), "真实桥应该报告在线且新鲜");
    }
}
