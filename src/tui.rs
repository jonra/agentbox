//! `agentbox tui`: a terminal dashboard in the spirit of Docker Desktop.
//!
//! Tabs for containers, images, volumes and compose projects; live status, CPU and
//! memory of each VM; start/stop, restart, delete, logs, exec, inspect, and opening a
//! published port in the browser. Slow actions run on a worker thread so the UI stays live.

use crate::compose;
use crate::container::{self, Container, ExecOpts, State};
use crate::registry;
use crate::store::{ImageMeta, Store};
use crate::vm::RunOpts;
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::BTreeMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const ACCENT: Color = Color::Rgb(29, 99, 237); // Docker-ish blue
const GREEN: Color = Color::Rgb(46, 160, 67);
const RED: Color = Color::Rgb(218, 54, 51);
const DIM: Color = Color::DarkGray;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Containers,
    Images,
    Volumes,
    Projects,
}

const TABS: [Tab; 4] = [Tab::Containers, Tab::Images, Tab::Volumes, Tab::Projects];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Containers => "Containers",
            Tab::Images => "Images",
            Tab::Volumes => "Volumes",
            Tab::Projects => "Compose",
        }
    }
    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }
}

struct CRow {
    c: Container,
    st: State,
    project: Option<String>,
    cpu: Option<f32>,
    mem_kb: Option<u64>,
}

struct VRow {
    name: String,
    users: Vec<String>,
}

struct PRow {
    name: String,
    dir: String,
    running: usize,
    total: usize,
}

#[derive(Clone)]
enum Action {
    DeleteContainer(String),
    DeleteImage(String),
    DeleteVolume(String),
    DownProject(String),
}

#[derive(Clone, Copy)]
enum PromptKind {
    Exec,
    Run,
    Pull,
}

enum Mode {
    Normal,
    Filter,
    Confirm(Action, String),
    Prompt(PromptKind, String, String), // kind, target, input
    Logs { name: String, scroll: u16, follow: bool },
    Text { title: String, body: String, scroll: u16 },
    Help,
}

struct App {
    store: Store,
    tab: Tab,
    states: [TableState; 4],
    containers: Vec<CRow>,
    images: Vec<ImageMeta>,
    volumes: Vec<VRow>,
    projects: Vec<PRow>,
    filter: String,
    project_filter: Option<String>,
    mode: Mode,
    status: Option<(String, bool, Instant)>, // text, is_error, when
    busy: usize,
    tx: Sender<(String, bool)>,
    rx: Receiver<(String, bool)>,
    text_tx: Sender<(String, String)>,
    text_rx: Receiver<(String, String)>,
    last_refresh: Instant,
    log_cache: (String, Vec<String>, Instant),
    quit: bool,
}

fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// %CPU and RSS (kB) of the VM processes, from one `ps` call.
fn vm_stats(pids: &[i32]) -> BTreeMap<i32, (f32, u64)> {
    let mut out = BTreeMap::new();
    if pids.is_empty() {
        return out;
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    if let Ok(o) = std::process::Command::new("ps").args(["-o", "pid=,%cpu=,rss=", "-p", &list]).output() {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if let [pid, cpu, rss] = f.as_slice() {
                if let (Ok(p), Ok(c), Ok(r)) = (pid.parse(), cpu.parse(), rss.parse()) {
                    out.insert(p, (c, r));
                }
            }
        }
    }
    out
}

impl App {
    fn new(store: Store) -> App {
        let (tx, rx) = channel();
        let (text_tx, text_rx) = channel();
        let mut states: [TableState; 4] = Default::default();
        for s in &mut states {
            s.select(Some(0));
        }
        let mut a = App {
            store,
            tab: Tab::Containers,
            states,
            containers: vec![],
            images: vec![],
            volumes: vec![],
            projects: vec![],
            filter: String::new(),
            project_filter: None,
            mode: Mode::Normal,
            status: None,
            busy: 0,
            tx,
            rx,
            text_tx,
            text_rx,
            last_refresh: Instant::now() - Duration::from_secs(10),
            log_cache: (String::new(), vec![], Instant::now() - Duration::from_secs(10)),
            quit: false,
        };
        a.refresh();
        a
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let list = container::list(&self.store).unwrap_or_default();
        let mut rows: Vec<CRow> = list
            .into_iter()
            .map(|c| {
                let st = c.state(&self.store);
                let project = c.config.labels.get(compose::L_PROJECT).cloned();
                CRow { c, st, project, cpu: None, mem_kb: None }
            })
            .collect();
        let pids: Vec<i32> = rows.iter().filter(|r| r.st.running()).filter_map(|r| r.st.pid).collect();
        let stats = vm_stats(&pids);
        for r in &mut rows {
            if let Some(p) = r.st.pid.filter(|_| r.st.running()) {
                if let Some((c, m)) = stats.get(&p) {
                    r.cpu = Some(*c);
                    r.mem_kb = Some(*m);
                }
            }
        }
        // Running first, then by project and name.
        rows.sort_by(|a, b| {
            b.st.running()
                .cmp(&a.st.running())
                .then(a.project.cmp(&b.project))
                .then(a.c.name.cmp(&b.c.name))
        });
        self.containers = rows;
        self.images = self.store.images().unwrap_or_default();
        let vols = self.store.volumes().unwrap_or_default();
        self.volumes = vols
            .into_iter()
            .map(|name| {
                let path = self
                    .store
                    .volume_dir(&name)
                    .ok()
                    .and_then(|d| std::fs::canonicalize(d).ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let users = self
                    .containers
                    .iter()
                    .filter(|r| !path.is_empty() && r.c.config.volumes.iter().any(|v| v.starts_with(&format!("{path}:"))))
                    .map(|r| r.c.name.clone())
                    .collect();
                VRow { name, users }
            })
            .collect();
        self.projects = compose::projects(&self.store)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, running, total, dir)| PRow { name, dir, running, total })
            .collect();
        for t in TABS {
            let n = self.len(t);
            let s = &mut self.states[t.index()];
            match s.selected() {
                Some(i) if n > 0 && i >= n => s.select(Some(n - 1)),
                None if n > 0 => s.select(Some(0)),
                _ => {}
            }
        }
    }

    fn matches(&self, text: &str) -> bool {
        self.filter.is_empty() || text.to_lowercase().contains(&self.filter.to_lowercase())
    }

    fn visible_containers(&self) -> Vec<&CRow> {
        self.containers
            .iter()
            .filter(|r| self.project_filter.as_ref().map(|p| r.project.as_ref() == Some(p)).unwrap_or(true))
            .filter(|r| self.matches(&format!("{} {} {}", r.c.name, r.c.image, r.project.clone().unwrap_or_default())))
            .collect()
    }
    fn visible_images(&self) -> Vec<&ImageMeta> {
        self.images.iter().filter(|i| self.matches(&i.reference)).collect()
    }
    fn visible_volumes(&self) -> Vec<&VRow> {
        self.volumes.iter().filter(|v| self.matches(&v.name)).collect()
    }
    fn visible_projects(&self) -> Vec<&PRow> {
        self.projects.iter().filter(|p| self.matches(&p.name)).collect()
    }

    fn len(&self, t: Tab) -> usize {
        match t {
            Tab::Containers => self.visible_containers().len(),
            Tab::Images => self.visible_images().len(),
            Tab::Volumes => self.visible_volumes().len(),
            Tab::Projects => self.visible_projects().len(),
        }
    }

    fn selected(&self) -> usize {
        self.states[self.tab.index()].selected().unwrap_or(0)
    }

    fn sel_container(&self) -> Option<Container> {
        self.visible_containers().get(self.selected()).map(|r| r.c.clone())
    }

    fn say(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }

    /// Run `f` on a worker thread; its result becomes the status line.
    fn spawn(&mut self, what: String, f: impl FnOnce(&Store) -> Result<String> + Send + 'static) {
        let store = self.store.clone();
        let tx = self.tx.clone();
        self.busy += 1;
        self.say(format!("{what}…"), false);
        std::thread::spawn(move || {
            let r = f(&store);
            let _ = match r {
                Ok(m) => tx.send((m, false)),
                Err(e) => tx.send((format!("{what}: {e:#}"), true)),
            };
        });
    }

    fn move_sel(&mut self, delta: i64) {
        let n = self.len(self.tab) as i64;
        if n == 0 {
            return;
        }
        let i = (self.selected() as i64 + delta).clamp(0, n - 1);
        self.states[self.tab.index()].select(Some(i as usize));
    }

    // ---- actions -------------------------------------------------------------

    fn toggle(&mut self) {
        match self.tab {
            Tab::Containers => {
                let Some(c) = self.sel_container() else { return };
                if c.state(&self.store).running() {
                    let n = c.name.clone();
                    self.spawn(format!("Stopping {n}"), move |s| {
                        container::stop(s, &c, Duration::from_secs(10))?;
                        Ok(format!("Stopped {n}"))
                    });
                } else {
                    let n = c.name.clone();
                    self.spawn(format!("Starting {n}"), move |s| {
                        container::start_detached(s, &c)?;
                        Ok(format!("Started {n}"))
                    });
                }
            }
            Tab::Projects => {
                let Some(p) = self.visible_projects().get(self.selected()).map(|p| (p.name.clone(), p.running > 0)) else { return };
                let (name, running) = p;
                self.spawn(format!("{} {name}", if running { "Stopping" } else { "Starting" }), move |s| {
                    let cs = project_order(s, &name)?;
                    if running {
                        for c in cs.iter().rev() {
                            container::stop(s, c, Duration::from_secs(10))?;
                        }
                        Ok(format!("Stopped project {name}"))
                    } else {
                        for c in cs.iter().filter(|c| !c.config.labels.contains_key(compose::L_ONEOFF)) {
                            if !c.state(s).running() {
                                container::start_detached(s, c)?;
                            }
                        }
                        Ok(format!("Started project {name}"))
                    }
                });
            }
            _ => {}
        }
    }

    fn restart(&mut self) {
        match self.tab {
            Tab::Containers => {
                let Some(c) = self.sel_container() else { return };
                let n = c.name.clone();
                self.spawn(format!("Restarting {n}"), move |s| {
                    container::stop(s, &c, Duration::from_secs(10))?;
                    container::start_detached(s, &c)?;
                    Ok(format!("Restarted {n}"))
                });
            }
            Tab::Projects => {
                let Some(name) = self.visible_projects().get(self.selected()).map(|p| p.name.clone()) else { return };
                self.spawn(format!("Restarting {name}"), move |s| {
                    let cs = project_order(s, &name)?;
                    for c in cs.iter().rev() {
                        container::stop(s, c, Duration::from_secs(10))?;
                    }
                    for c in cs.iter().filter(|c| !c.config.labels.contains_key(compose::L_ONEOFF)) {
                        container::start_detached(s, c)?;
                    }
                    Ok(format!("Restarted project {name}"))
                });
            }
            Tab::Images => {
                if let Some(i) = self.visible_images().get(self.selected()).map(|i| i.reference.clone()) {
                    self.mode = Mode::Prompt(PromptKind::Run, i, String::new());
                }
            }
            _ => {}
        }
    }

    fn delete(&mut self) {
        let sel = self.selected();
        let action = match self.tab {
            Tab::Containers => self.sel_container().map(|c| (Action::DeleteContainer(c.name.clone()), format!("Delete container {}?", c.name))),
            Tab::Images => self.visible_images().get(sel).map(|i| (Action::DeleteImage(i.reference.clone()), format!("Delete image {}?", i.reference))),
            Tab::Volumes => self.visible_volumes().get(sel).map(|v| (Action::DeleteVolume(v.name.clone()), format!("Delete volume {} and its data?", v.name))),
            Tab::Projects => self
                .visible_projects()
                .get(sel)
                .map(|p| (Action::DownProject(p.name.clone()), format!("Stop and remove the {} containers of {}? (volumes are kept)", p.total, p.name))),
        };
        if let Some((a, q)) = action {
            self.mode = Mode::Confirm(a, q);
        }
    }

    fn confirm(&mut self, a: Action) {
        match a {
            Action::DeleteContainer(n) => self.spawn(format!("Deleting {n}"), move |s| {
                let c = container::find(s, &n)?;
                container::remove(s, &c, true)?;
                Ok(format!("Deleted {n}"))
            }),
            Action::DeleteImage(r) => self.spawn(format!("Deleting {r}"), move |s| {
                let users: Vec<String> = container::list(s)?.into_iter().filter(|c| c.image == r).map(|c| c.name).collect();
                if !users.is_empty() {
                    anyhow::bail!("used by {}", users.join(", "));
                }
                s.remove_image(&r)?;
                Ok(format!("Deleted {r}"))
            }),
            Action::DeleteVolume(v) => {
                if let Some(row) = self.volumes.iter().find(|x| x.name == v) {
                    if !row.users.is_empty() {
                        let msg = format!("Volume {v} is used by {}", row.users.join(", "));
                        self.say(msg, true);
                        return;
                    }
                }
                self.spawn(format!("Deleting volume {v}"), move |s| {
                    crate::store::remove_tree(&s.volume_dir(&v)?)?;
                    Ok(format!("Deleted volume {v}"))
                })
            }
            Action::DownProject(p) => self.spawn(format!("Removing {p}"), move |s| {
                let cs = project_order(s, &p)?;
                for c in cs.iter().rev() {
                    container::stop(s, c, Duration::from_secs(10))?;
                    container::remove(s, c, true)?;
                }
                Ok(format!("Removed the containers of {p}"))
            }),
        }
    }

    fn inspect(&mut self) {
        let (title, body) = match self.tab {
            Tab::Containers => {
                let Some(c) = self.sel_container() else { return };
                let st = c.state(&self.store);
                let v = serde_json::json!({
                    "Id": c.id, "Name": c.name, "Image": c.image, "Created": c.created,
                    "State": st, "Config": c.config, "Path": c.dir(&self.store),
                });
                (format!("inspect {}", c.name), serde_json::to_string_pretty(&v).unwrap_or_default())
            }
            Tab::Images => {
                let Some(i) = self.visible_images().get(self.selected()).cloned().cloned() else { return };
                (format!("inspect {}", i.reference), serde_json::to_string_pretty(&i).unwrap_or_default())
            }
            Tab::Volumes => {
                let Some(v) = self.visible_volumes().get(self.selected()).map(|v| (v.name.clone(), v.users.clone())) else { return };
                let path = self.store.volume_dir(&v.0).map(|p| p.display().to_string()).unwrap_or_default();
                let size = dir_size(&path);
                (
                    format!("volume {}", v.0),
                    format!("Path:  {path}\nSize:  {}\nUsed by: {}", human(size), if v.1.is_empty() { "-".into() } else { v.1.join(", ") }),
                )
            }
            Tab::Projects => {
                let Some(p) = self.visible_projects().get(self.selected()).map(|p| p.name.clone()) else { return };
                self.project_filter = Some(p);
                self.tab = Tab::Containers;
                self.states[0].select(Some(0));
                return;
            }
        };
        self.mode = Mode::Text { title, body, scroll: 0 };
    }

    fn open_port(&mut self) {
        let Some(c) = self.sel_container() else { return };
        let label = c.config.labels.get(compose::L_PUBLISHED).cloned();
        let host = match label {
            Some(l) => l.split(',').next().and_then(|m| m.split(':').nth(1)).and_then(|m| m.split("->").next()).map(String::from),
            None => c.config.ports.first().and_then(|p| p.split(':').next()).map(String::from),
        };
        match host {
            Some(h) => {
                let url = format!("http://localhost:{h}");
                let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }).arg(&url).spawn();
                self.say(format!("Opened {url}"), false);
            }
            None => self.say(format!("{} has no published port", c.name), true),
        }
    }

    fn submit_prompt(&mut self, kind: PromptKind, target: String, input: String) {
        match kind {
            PromptKind::Exec => {
                let Ok(argv) = crate::build::shell_split(&input) else {
                    self.say("couldn't parse the command", true);
                    return;
                };
                if argv.is_empty() {
                    return;
                }
                let tx = self.text_tx.clone();
                let store = self.store.clone();
                self.say(format!("Running in {target}…"), false);
                std::thread::spawn(move || {
                    let r = container::find(&store, &target).and_then(|c| {
                        container::exec_captured(&store, &c, &ExecOpts { command: argv.clone(), env: vec![], workdir: None, timeout: Some(Duration::from_secs(120)) })
                    });
                    let body = match r {
                        Ok(r) => {
                            let mut b = r.stdout;
                            if !r.stderr.is_empty() {
                                b.push_str(&format!("\n--- stderr ---\n{}", r.stderr));
                            }
                            b.push_str(&format!(
                                "\n--- exit {} in {} ms ---",
                                r.exit_code.map(|x| x.to_string()).unwrap_or_else(|| "timeout".into()),
                                r.duration_ms
                            ));
                            b
                        }
                        Err(e) => format!("error: {e:#}"),
                    };
                    let _ = tx.send((format!("{target}: {}", argv.join(" ")), body));
                });
            }
            PromptKind::Run => {
                let argv = crate::build::shell_split(&input).unwrap_or_default();
                self.spawn(format!("Starting {target}"), move |s| {
                    let c = container::create(s, &RunOpts { image: target.clone(), command: argv, ..Default::default() })?;
                    container::start_detached(s, &c)?;
                    Ok(format!("Started {} from {target}", c.name))
                });
            }
            PromptKind::Pull => {
                let img = input.trim().to_string();
                if img.is_empty() {
                    return;
                }
                self.spawn(format!("Pulling {img}"), move |s| {
                    let m = registry::pull(s, &img, true)?;
                    Ok(format!("Pulled {}", m.reference))
                });
            }
        }
    }

    // ---- input ---------------------------------------------------------------

    fn key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = match mode {
            Mode::Help => Mode::Normal,
            Mode::Confirm(a, q) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm(a);
                    Mode::Normal
                }
                KeyCode::Char('n') | KeyCode::Esc => Mode::Normal,
                _ => Mode::Confirm(a, q),
            },
            Mode::Filter => match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    Mode::Normal
                }
                KeyCode::Enter => Mode::Normal,
                KeyCode::Backspace => {
                    self.filter.pop();
                    Mode::Filter
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.states[self.tab.index()].select(Some(0));
                    Mode::Filter
                }
                _ => Mode::Filter,
            },
            Mode::Prompt(kind, target, mut input) => match k.code {
                KeyCode::Esc => Mode::Normal,
                KeyCode::Enter => {
                    self.submit_prompt(kind, target, input);
                    Mode::Normal
                }
                KeyCode::Backspace => {
                    input.pop();
                    Mode::Prompt(kind, target, input)
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    Mode::Prompt(kind, target, input)
                }
                _ => Mode::Prompt(kind, target, input),
            },
            Mode::Logs { name, scroll, follow } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => Mode::Normal,
                KeyCode::Up | KeyCode::Char('k') => Mode::Logs { name, scroll: scroll.saturating_add(1), follow: false },
                KeyCode::Down | KeyCode::Char('j') => {
                    let s = scroll.saturating_sub(1);
                    Mode::Logs { name, scroll: s, follow: s == 0 }
                }
                KeyCode::PageUp => Mode::Logs { name, scroll: scroll.saturating_add(20), follow: false },
                KeyCode::PageDown => {
                    let s = scroll.saturating_sub(20);
                    Mode::Logs { name, scroll: s, follow: s == 0 }
                }
                KeyCode::Char('f') | KeyCode::End | KeyCode::Char('G') => Mode::Logs { name, scroll: 0, follow: true },
                KeyCode::Home | KeyCode::Char('g') => Mode::Logs { name, scroll: u16::MAX / 2, follow: false },
                _ => Mode::Logs { name, scroll, follow },
            },
            Mode::Text { title, body, scroll } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => Mode::Normal,
                KeyCode::Up | KeyCode::Char('k') => Mode::Text { title, body, scroll: scroll.saturating_sub(1) },
                KeyCode::Down | KeyCode::Char('j') => Mode::Text { title, body, scroll: scroll.saturating_add(1) },
                KeyCode::PageUp => Mode::Text { title, body, scroll: scroll.saturating_sub(20) },
                KeyCode::PageDown => Mode::Text { title, body, scroll: scroll.saturating_add(20) },
                _ => Mode::Text { title, body, scroll },
            },
            Mode::Normal => {
                self.normal_key(k);
                std::mem::replace(&mut self.mode, Mode::Normal)
            }
        };
    }

    fn normal_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                } else {
                    self.project_filter = None;
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = TABS[(c as u8 - b'1') as usize];
            }
            KeyCode::Tab | KeyCode::Right => self.tab = TABS[(self.tab.index() + 1) % TABS.len()],
            KeyCode::BackTab | KeyCode::Left => self.tab = TABS[(self.tab.index() + TABS.len() - 1) % TABS.len()],
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-100_000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(100_000),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('R') => {
                self.refresh();
                self.say("Refreshed", false);
            }
            KeyCode::Char('s') => self.toggle(),
            KeyCode::Char('r') => self.restart(),
            KeyCode::Char('d') | KeyCode::Delete => self.delete(),
            KeyCode::Char('i') => self.inspect(),
            KeyCode::Char('p') if self.tab == Tab::Images => self.mode = Mode::Prompt(PromptKind::Pull, String::new(), String::new()),
            KeyCode::Enter | KeyCode::Char('l') => match self.tab {
                Tab::Containers => {
                    if let Some(c) = self.sel_container() {
                        self.mode = Mode::Logs { name: c.name, scroll: 0, follow: true };
                    }
                }
                _ => self.inspect(),
            },
            KeyCode::Char('x') | KeyCode::Char('e') if self.tab == Tab::Containers => {
                if let Some(c) = self.sel_container() {
                    if c.state(&self.store).running() {
                        self.mode = Mode::Prompt(PromptKind::Exec, c.name, String::new());
                    } else {
                        self.say(format!("{} is not running", c.name), true);
                    }
                }
            }
            KeyCode::Char('o') if self.tab == Tab::Containers => self.open_port(),
            _ => {}
        }
    }

    // ---- drawing -------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [header, tabs, body, footer, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_tabs(f, tabs);
        match &self.mode {
            Mode::Logs { name, scroll, follow } => {
                let (n, s, fo) = (name.clone(), *scroll, *follow);
                self.draw_logs(f, body, &n, s, fo);
            }
            _ => match self.tab {
                Tab::Containers => self.draw_containers(f, body),
                Tab::Images => self.draw_images(f, body),
                Tab::Volumes => self.draw_volumes(f, body),
                Tab::Projects => self.draw_projects(f, body),
            },
        }
        self.draw_footer(f, footer);
        self.draw_hints(f, hints);
        match &self.mode {
            Mode::Confirm(_, q) => popup(f, "Confirm", &format!("{q}\n\n[y] yes   [n] no"), 60, 7, RED),
            Mode::Prompt(kind, target, input) => {
                let (title, label) = match kind {
                    PromptKind::Exec => (format!("exec in {target}"), "Command (no TTY; e.g. ls -la /app):"),
                    PromptKind::Run => (format!("run {target}"), "Command (empty = image default), starts in the background:"),
                    PromptKind::Pull => ("pull image".to_string(), "Image (e.g. postgres:16, ghcr.io/org/app:tag):"),
                };
                popup(f, &title, &format!("{label}\n\n> {input}█\n\n[Enter] run   [Esc] cancel"), 70, 8, ACCENT);
            }
            Mode::Text { title, body, scroll } => {
                let area = centered(f.area(), 90, 85);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(body.as_str())
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0))
                        .block(bordered(title, ACCENT).title_bottom(" ↑↓ scroll · Esc close ")),
                    area,
                );
            }
            Mode::Help => popup(f, "Keys", HELP, 64, 24, ACCENT),
            _ => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let running = self.containers.iter().filter(|r| r.st.running()).count();
        let cpu: f32 = self.containers.iter().filter_map(|r| r.cpu).sum();
        let mem: u64 = self.containers.iter().filter_map(|r| r.mem_kb).sum();
        let line = Line::from(vec![
            Span::styled(" ◆ agentbox ", Style::new().fg(Color::White).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled(format!("{running} running"), Style::new().fg(GREEN).bold()),
            Span::styled(format!(" · {} containers · {} images · {} volumes", self.containers.len(), self.images.len(), self.volumes.len()), Style::new().fg(DIM)),
            Span::styled(format!("   CPU {cpu:.0}%  MEM {}", human(mem * 1024)), Style::new().fg(Color::Gray)),
            if self.busy > 0 { Span::styled("   ⟳ working", Style::new().fg(Color::Yellow)) } else { Span::raw("") },
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let titles: Vec<Line> = TABS
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let n = match t {
                    Tab::Containers => self.containers.len(),
                    Tab::Images => self.images.len(),
                    Tab::Volumes => self.volumes.len(),
                    Tab::Projects => self.projects.len(),
                };
                Line::from(format!(" {} {} ({n}) ", i + 1, t.title()))
            })
            .collect();
        f.render_widget(
            Tabs::new(titles)
                .select(self.tab.index())
                .style(Style::new().fg(Color::Gray))
                .highlight_style(Style::new().fg(Color::White).bg(ACCENT).bold())
                .divider(" ")
                .block(Block::new().borders(ratatui::widgets::Borders::BOTTOM).border_style(Style::new().fg(DIM))),
            area,
        );
    }

    fn table_block(&self, title: &str) -> Block<'static> {
        let mut t = title.to_string();
        if let Some(p) = &self.project_filter {
            if self.tab == Tab::Containers {
                t = format!("{t} · project {p} (Esc to show all)");
            }
        }
        if !self.filter.is_empty() || matches!(self.mode, Mode::Filter) {
            t = format!("{t} · filter: {}", self.filter);
        }
        bordered(&t, DIM)
    }

    fn draw_containers(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .visible_containers()
            .iter()
            .map(|r| {
                let (dot, color) = if r.st.running() {
                    ("●", GREEN)
                } else if r.st.status == "exited" && r.st.exit_code.unwrap_or(0) != 0 {
                    ("●", RED)
                } else {
                    ("○", DIM)
                };
                Row::new(vec![
                    Cell::from(Span::styled(dot, Style::new().fg(color))),
                    Cell::from(r.c.name.clone()).style(Style::new().bold()),
                    Cell::from(r.c.image.clone()),
                    Cell::from(crate::status_text(&self.store, &r.c)).style(Style::new().fg(if r.st.running() { GREEN } else { Color::Gray })),
                    Cell::from(compose::ports_text(&r.c).replace("0.0.0.0:", "")),
                    Cell::from(r.cpu.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "–".into())),
                    Cell::from(r.mem_kb.map(|m| human(m * 1024)).unwrap_or_else(|| "–".into())),
                    Cell::from(r.project.clone().unwrap_or_default()).style(Style::new().fg(Color::Cyan)),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let t = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(3),
                Constraint::Fill(3),
                Constraint::Length(24),
                Constraint::Fill(2),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Fill(1),
            ],
        )
        .header(header(&["", "NAME", "IMAGE", "STATUS", "PORTS", "CPU", "MEM", "PROJECT"]))
        .row_highlight_style(Style::new().bg(Color::Rgb(40, 52, 80)))
        .highlight_symbol("▌")
        .block(self.table_block("Containers"));
        f.render_stateful_widget(t, area, &mut self.states[0]);
        if empty {
            hint_empty(f, area, "No containers. Start one with `agentbox run -d …`, `agentbox compose up -d`, or run an image from the Images tab.");
        }
    }

    fn draw_images(&mut self, f: &mut Frame, area: Rect) {
        let used: Vec<String> = self.containers.iter().map(|r| r.c.image.clone()).collect();
        let rows: Vec<Row> = self
            .visible_images()
            .iter()
            .map(|i| {
                let (repo, tag) = match i.reference.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                    Some((r, t)) => (r.to_string(), t.to_string()),
                    None => (i.reference.clone(), String::new()),
                };
                let in_use = used.contains(&i.reference);
                Row::new(vec![
                    Cell::from(Span::styled(if in_use { "●" } else { "○" }, Style::new().fg(if in_use { GREEN } else { DIM }))),
                    Cell::from(repo).style(Style::new().bold()),
                    Cell::from(tag),
                    Cell::from(i.digest.get(7..19).unwrap_or("").to_string()).style(Style::new().fg(DIM)),
                    Cell::from(human(i.size_bytes)),
                    Cell::from(i.exposed_ports.join(" ")),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let t = Table::new(rows, [Constraint::Length(1), Constraint::Fill(4), Constraint::Fill(2), Constraint::Length(13), Constraint::Length(9), Constraint::Fill(1)])
            .header(header(&["", "REPOSITORY", "TAG", "IMAGE ID", "SIZE", "EXPOSES"]))
            .row_highlight_style(Style::new().bg(Color::Rgb(40, 52, 80)))
            .highlight_symbol("▌")
            .block(self.table_block("Images  (● in use)"));
        f.render_stateful_widget(t, area, &mut self.states[1]);
        if empty {
            hint_empty(f, area, "No images. Press p to pull one, or use `agentbox build` / `agentbox load`.");
        }
    }

    fn draw_volumes(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .visible_volumes()
            .iter()
            .map(|v| {
                Row::new(vec![
                    Cell::from(Span::styled(if v.users.is_empty() { "○" } else { "●" }, Style::new().fg(if v.users.is_empty() { DIM } else { GREEN }))),
                    Cell::from(v.name.clone()).style(Style::new().bold()),
                    Cell::from(if v.users.is_empty() { "–".to_string() } else { v.users.join(", ") }),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let t = Table::new(rows, [Constraint::Length(1), Constraint::Fill(2), Constraint::Fill(3)])
            .header(header(&["", "NAME", "USED BY"]))
            .row_highlight_style(Style::new().bg(Color::Rgb(40, 52, 80)))
            .highlight_symbol("▌")
            .block(self.table_block("Volumes"));
        f.render_stateful_widget(t, area, &mut self.states[2]);
        if empty {
            hint_empty(f, area, "No volumes. Named volumes appear here (`-v name:/path`, compose `volumes:`).");
        }
    }

    fn draw_projects(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .visible_projects()
            .iter()
            .map(|p| {
                let color = if p.running == p.total { GREEN } else if p.running > 0 { Color::Yellow } else { DIM };
                Row::new(vec![
                    Cell::from(Span::styled(if p.running > 0 { "●" } else { "○" }, Style::new().fg(color))),
                    Cell::from(p.name.clone()).style(Style::new().bold()),
                    Cell::from(format!("{}/{} running", p.running, p.total)).style(Style::new().fg(color)),
                    Cell::from(p.dir.clone()).style(Style::new().fg(DIM)),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let t = Table::new(rows, [Constraint::Length(1), Constraint::Fill(2), Constraint::Length(14), Constraint::Fill(4)])
            .header(header(&["", "PROJECT", "STATUS", "DIRECTORY"]))
            .row_highlight_style(Style::new().bg(Color::Rgb(40, 52, 80)))
            .highlight_symbol("▌")
            .block(self.table_block("Compose projects"));
        f.render_stateful_widget(t, area, &mut self.states[3]);
        if empty {
            hint_empty(f, area, "No compose projects. Run `agentbox compose up -d` in a directory with a compose.yaml.");
        }
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect, name: &str, scroll: u16, follow: bool) {
        if self.log_cache.0 != name || self.log_cache.2.elapsed() > Duration::from_millis(500) {
            let mut buf = vec![];
            if let Ok(c) = container::find(&self.store, name) {
                let _ = container::logs(&self.store, &c, false, Some(5000), &mut buf);
            }
            let lines = String::from_utf8_lossy(&buf).lines().map(strip_ansi).collect();
            self.log_cache = (name.to_string(), lines, Instant::now());
        }
        let lines = &self.log_cache.1;
        let h = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(h);
        let back = (scroll as usize).min(max_scroll);
        let start = lines.len().saturating_sub(h + back);
        let text: Vec<Line> = lines[start..(start + h).min(lines.len())].iter().map(|l| Line::raw(l.clone())).collect();
        let state = if follow { "following".to_string() } else { format!("{back} lines up") };
        f.render_widget(
            Paragraph::new(text).block(
                bordered(&format!("logs {name} · {} lines · {state}", lines.len()), ACCENT)
                    .title_bottom(" ↑↓ PgUp/PgDn scroll · f follow · g top · Esc back "),
            ),
            area,
        );
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        let line = match &self.status {
            Some((m, err, t)) if t.elapsed() < Duration::from_secs(if *err { 12 } else { 6 }) || self.busy > 0 => {
                Line::from(Span::styled(format!(" {m}"), Style::new().fg(if *err { RED } else { Color::Yellow })))
            }
            _ => {
                let detail = match self.tab {
                    Tab::Containers => self.sel_container().map(|c| {
                        format!(" {} · {} · {} · created {} ago", c.short_id(), crate::command_text(&self.store, &c), c.config.memory_mib.map(|m| format!("{m} MiB")).unwrap_or_else(|| "1024 MiB".into()), crate::ago(c.created))
                    }),
                    Tab::Images => self.visible_images().get(self.selected()).map(|i| format!(" {} · {}", i.digest, if i.cmd.is_empty() { i.entrypoint.join(" ") } else { i.cmd.join(" ") })),
                    Tab::Volumes => None,
                    Tab::Projects => self.visible_projects().get(self.selected()).map(|p| format!(" Enter: show its containers · {}", p.dir)),
                };
                Line::from(Span::styled(detail.unwrap_or_default(), Style::new().fg(DIM)))
            }
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_hints(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = match (&self.mode, self.tab) {
            (Mode::Filter, _) => &[("type", "filter"), ("Enter", "keep"), ("Esc", "clear")],
            (Mode::Logs { .. }, _) => &[("↑↓", "scroll"), ("f", "follow"), ("Esc", "back")],
            (_, Tab::Containers) => &[("s", "start/stop"), ("r", "restart"), ("l", "logs"), ("x", "exec"), ("o", "open"), ("i", "inspect"), ("d", "delete"), ("/", "filter"), ("?", "help"), ("q", "quit")],
            (_, Tab::Images) => &[("r", "run"), ("p", "pull"), ("i", "inspect"), ("d", "delete"), ("/", "filter"), ("?", "help"), ("q", "quit")],
            (_, Tab::Volumes) => &[("i", "details"), ("d", "delete"), ("/", "filter"), ("?", "help"), ("q", "quit")],
            (_, Tab::Projects) => &[("Enter", "containers"), ("s", "start/stop"), ("r", "restart"), ("d", "down"), ("?", "help"), ("q", "quit")],
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, v) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Gray)));
            spans.push(Span::styled(format!(" {v}  "), Style::new().fg(Color::Gray)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// Containers of a compose project in service start order (by creation time as a proxy).
fn project_order(s: &Store, name: &str) -> Result<Vec<Container>> {
    let mut cs = compose::project_containers(s, name)?;
    cs.sort_by_key(|c| c.created);
    Ok(cs)
}

fn dir_size(p: &str) -> u64 {
    std::process::Command::new("du")
        .args(["-sk", p])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().and_then(|n| n.parse::<u64>().ok()))
        .map(|k| k * 1024)
        .unwrap_or(0)
}

/// Drop terminal escape sequences from log lines.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn header(cols: &[&str]) -> Row<'static> {
    Row::new(cols.iter().map(|c| Cell::from(c.to_string())).collect::<Vec<_>>()).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD)).bottom_margin(0)
}

fn bordered(title: &str, color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(format!(" {title} "), Style::new().fg(Color::White).bold()))
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn popup(f: &mut Frame, title: &str, body: &str, w: u16, h: u16, color: Color) {
    let a = f.area();
    let w = w.min(a.width.saturating_sub(2));
    let h = h.min(a.height.saturating_sub(2));
    let r = Rect { x: a.x + (a.width - w) / 2, y: a.y + (a.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(body.to_string()).wrap(Wrap { trim: false }).block(bordered(title, color)), r);
}

fn hint_empty(f: &mut Frame, area: Rect, msg: &str) {
    if area.height < 5 {
        return;
    }
    let r = Rect { x: area.x + 2, y: area.y + 3, width: area.width.saturating_sub(4), height: 2 };
    f.render_widget(Paragraph::new(msg.to_string()).style(Style::new().fg(DIM)).wrap(Wrap { trim: true }), r);
}

const HELP: &str = "\
Everywhere
  1-4 / Tab / ← →   switch tab
  ↑ ↓ / j k         move        PgUp PgDn g G  jump
  /                 filter      Esc  clear filter
  R                 refresh     q  quit

Containers
  s  start / stop        r  restart       d  delete
  l  Enter  logs         x  exec a command (no TTY)
  i  inspect (JSON)      o  open the published port

Images
  r  run (in the background)   p  pull   d  delete   i  inspect

Volumes
  i  details and size          d  delete (when unused)

Compose
  Enter  show the project's containers
  s  start / stop all    r  restart all   d  down (keeps volumes)";

pub fn run(store: Store) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, store);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, store: Store) -> Result<()> {
    // `ratatui::init` also installs a panic hook that restores the terminal.
    let mut app = App::new(store);
    while !app.quit {
        while let Ok((msg, err)) = app.rx.try_recv() {
            app.busy = app.busy.saturating_sub(1);
            app.say(msg, err);
            app.refresh();
        }
        while let Ok((title, body)) = app.text_rx.try_recv() {
            app.status = None;
            app.mode = Mode::Text { title, body, scroll: 0 };
        }
        if app.last_refresh.elapsed() > Duration::from_millis(1500) {
            app.refresh();
        }
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                app.key(k);
            }
        }
    }
    Ok(())
}
