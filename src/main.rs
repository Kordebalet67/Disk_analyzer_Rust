use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Константы макета
// ---------------------------------------------------------------------------
const NAME_W: f32 = 280.0;
const SIZE_W: f32 = 90.0;
const BAR_W: f32 = 160.0;
const PCT_W: f32 = 55.0;
const COUNT_W: f32 = 70.0;
const TYPE_W: f32 = 70.0;
const COL_GAP: f32 = 10.0;
const ROW_H: f32 = 20.0;

// ---------------------------------------------------------------------------
// Модель данных
// ---------------------------------------------------------------------------
#[derive(Clone)]
enum Entry {
    File { name: String, size: u64 },
    Dir(Arc<DirNode>),
}

impl Entry {
    fn name(&self) -> &str {
        match self {
            Entry::File { name, .. } => name,
            Entry::Dir(d) => &d.name,
        }
    }
    fn size(&self) -> u64 {
        match self {
            Entry::File { size, .. } => *size,
            Entry::Dir(d) => d.size,
        }
    }
    fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir(_))
    }
}

struct DirNode {
    name: String,
    path: PathBuf,
    size: u64,
    count: u64,
    children: Vec<Entry>,
    has_error: bool,
}

// ---------------------------------------------------------------------------
// Прогресс (атомарный, без локов)
// ---------------------------------------------------------------------------
#[derive(Default)]
struct Progress {
    files: AtomicU64,
    bytes: AtomicU64,
}

// ---------------------------------------------------------------------------
// Быстрый параллельный сканер
// ---------------------------------------------------------------------------
fn scan_tree(path: &Path, progress: &Arc<Progress>) -> DirNode {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut has_error = false;
    let mut children: Vec<Entry> = Vec::new();
    let mut dir_paths: Vec<PathBuf> = Vec::new();

    match std::fs::read_dir(path) {
        Ok(rd) => {
            for e in rd {
                match e {
                    Ok(e) => {
                        let p = e.path();
                        match p.symlink_metadata() {
                            Ok(meta) => {
                                let fname = p
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                if meta.is_dir() {
                                    dir_paths.push(p);
                                } else {
                                    let size = meta.len();
                                    progress.files.fetch_add(1, Ordering::Relaxed);
                                    progress.bytes.fetch_add(size, Ordering::Relaxed);
                                    children.push(Entry::File { name: fname, size });
                                }
                            }
                            Err(_) => has_error = true,
                        }
                    }
                    Err(_) => has_error = true,
                }
            }
        }
        Err(_) => has_error = true,
    }

    // Параллельная рекурсия по подпапкам
    let sub_nodes: Vec<DirNode> = dir_paths
        .par_iter()
        .map(|p| scan_tree(p, progress))
        .collect();

    for node in sub_nodes {
        has_error |= node.has_error;
        children.push(Entry::Dir(Arc::new(node)));
    }

    let mut total_size = 0u64;
    let mut total_count = 0u64;
    for c in &children {
        total_size += c.size();
        match c {
            Entry::Dir(d) => total_count += d.count,
            Entry::File { .. } => total_count += 1,
        }
    }
    children.sort_by(|a, b| b.size().cmp(&a.size()));

    DirNode {
        name,
        path: path.to_path_buf(),
        size: total_size,
        count: total_count,
        children,
        has_error,
    }
}

// ---------------------------------------------------------------------------
// Утилиты форматирования
// ---------------------------------------------------------------------------
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", size, UNITS[i])
    }
}

fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut res = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            res.push(' ');
        }
        res.push(c);
    }
    res.chars().rev().collect()
}

fn reveal_path(path: &Path, is_dir: bool) {
    #[cfg(target_os = "windows")]
    {
        if is_dir {
            let _ = open::that(path);
        } else {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(path)
                .spawn()
                .ok();
        }
    }
    #[cfg(target_os = "macos")]
    {
        if is_dir {
            let _ = open::that(path);
        } else {
            std::process::Command::new("open").arg("-R").arg(path).spawn().ok();
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if is_dir {
            path.to_path_buf()
        } else {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.to_path_buf())
        };
        let _ = open::that(target);
    }
}

// ---------------------------------------------------------------------------
// Состояние приложения
// ---------------------------------------------------------------------------
struct App {
    root_node: Option<Arc<DirNode>>,
    stack: Vec<Arc<DirNode>>,
    view_entries: Vec<Entry>,
    view_dirty: bool,
    sort_by_name: bool,
    scanning: bool,
    scan_progress: Arc<Progress>,
    scan_start: Option<Instant>,
    result: Arc<Mutex<Option<DirNode>>>,
    enter_request: Arc<Mutex<Option<Arc<DirNode>>>>,
    display_limit: usize,
}

impl App {
    fn new() -> Self {
        Self {
            root_node: None,
            stack: Vec::new(),
            view_entries: Vec::new(),
            view_dirty: false,
            sort_by_name: false,
            scanning: false,
            scan_progress: Arc::new(Progress::default()),
            scan_start: None,
            result: Arc::new(Mutex::new(None)),
            enter_request: Arc::new(Mutex::new(None)),
            display_limit: 5000,
        }
    }

    fn current_dir(&self) -> Option<&Arc<DirNode>> {
        self.stack.last()
    }

    fn rebuild_view(&mut self) {
        if let Some(dir) = self.current_dir() {
            let mut v = dir.children.clone();
            if self.sort_by_name {
                v.sort_by(|a, b| {
                    b.is_dir()
                        .cmp(&a.is_dir())
                        .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
                });
            }
            self.view_entries = v;
        } else {
            self.view_entries.clear();
        }
        self.view_dirty = false;
    }

    fn enter_dir(&mut self, n: Arc<DirNode>) {
        self.stack.push(n);
        self.view_dirty = true;
    }

    fn go_up(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.view_dirty = true;
        }
    }

    fn jump_to(&mut self, i: usize) {
        if i < self.stack.len() {
            self.stack.truncate(i + 1);
            self.view_dirty = true;
        }
    }

    fn trigger_scan(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.root_node = None;
            self.stack.clear();
            self.view_entries.clear();
            self.view_dirty = false;
            self.scanning = true;
            self.scan_start = Some(Instant::now());
            self.scan_progress = Arc::new(Progress::default());

            let progress = self.scan_progress.clone();
            let result = self.result.clone();
            let ctx2 = ctx.clone();

            std::thread::spawn(move || {
                let node = scan_tree(&path, &progress);
                *result.lock().unwrap() = Some(node);
                ctx2.request_repaint();
            });
        }
    }
}

// ---------------------------------------------------------------------------
// GUI
// ---------------------------------------------------------------------------
fn draw_right_text(ui: &mut egui::Ui, width: f32, text: String, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::hover());
    let pos = rect.right_center() - egui::vec2(2.0, 0.0);
    ui.painter().text(
        text,
        Align2::RIGHT_CENTER,
        pos,
        FontId::monospace(13.0),
        color,
    );
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- Завершение сканирования ---
        if self.scanning {
            let mut r = self.result.lock().unwrap();
            if let Some(node) = r.take() {
                drop(r);
                self.scanning = false;
                let arc = Arc::new(node);
                self.root_node = Some(arc.clone());
                self.stack.clear();
                self.stack.push(arc);
                self.view_dirty = true;
            } else {
                drop(r);
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        // --- Горячие клавиши ---
        let mut open_req = false;
        let mut up_req = false;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::CTRL, egui::Key::O) {
                open_req = true;
            }
            if i.key_pressed(egui::Key::Backspace) {
                up_req = true;
            }
            if i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft) {
                up_req = true;
            }
        });
        if open_req && !self.scanning {
            self.trigger_scan(ctx);
        }
        if up_req {
            self.go_up();
        }

        if self.view_dirty {
            self.rebuild_view();
        }

        // --- Верхняя панель ---
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Открыть папку…").clicked() && !self.scanning {
                    self.trigger_scan(ctx);
                }
                let can_up = self.stack.len() > 1 && !self.scanning;
                if ui.add_enabled(can_up, egui::Button::new("Вверх")).clicked() {
                    self.go_up();
                }
                if self.scanning {
                    ui.spinner();
                    let files = self.scan_progress.files.load(Ordering::Relaxed);
                    let bytes = self.scan_progress.bytes.load(Ordering::Relaxed);
                    let elapsed = self.scan_start.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                    ui.label(format!(
                        "Сканирование… файлов: {} • {} • {} с",
                        fmt_int(files),
                        human_size(bytes),
                        elapsed
                    ));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(dir) = self.current_dir() {
                        ui.label(RichText::new(human_size(dir.size)).strong());
                    }
                });
            });

            // Хлебные крошки
            ui.horizontal_wrapped(|ui| {
                let len = self.stack.len();
                for i in 0..len {
                    if i > 0 {
                        ui.label("›");
                    }
                    let node = self.stack[i].clone();
                    let text = if node.name.chars().count() > 25 {
                        let s: String = node.name.chars().take(24).collect();
                        format!("{}…", s)
                    } else {
                        node.name.clone()
                    };
                    if i == len - 1 {
                        ui.label(RichText::new(text).strong());
                    } else if ui.link(text).clicked() {
                        self.jump_to(i);
                    }
                }
            });
            ui.separator();
        });

        // --- Нижняя панель ---
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(dir) = self.current_dir() {
                    ui.label(format!(
                        "Объектов: {} • Размер: {}",
                        fmt_int(dir.children.len() as u64),
                        human_size(dir.size)
                    ));
                    if dir.has_error {
                        ui.label(RichText::new("⚠ есть недоступные области").color(Color32::ORANGE));
                    }
                } else if !self.scanning {
                    ui.label("Выберите папку для анализа (Ctrl+O)");
                }
            });
        });

        // --- Центральная панель ---
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.root_node.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("DiskAnalyzer");
                    if self.scanning {
                        ui.spinner();
                        ui.label("Сканирование…");
                    } else {
                        ui.label("Нажмите «Открыть папку…» (Ctrl+O) для начала анализа");
                    }
                });
                return;
            }

            let text_color = ui.visuals().text_color();
            let secondary = Color32::from_gray(120);

            // Заголовок таблицы
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = COL_GAP;
                let name_txt = if self.sort_by_name { "Имя ▲" } else { "Имя" };
                if ui.add_sized([NAME_W, ROW_H], egui::Button::new(name_txt)).clicked() {
                    self.sort_by_name = true;
                    self.view_dirty = true;
                }
                let size_txt = if !self.sort_by_name { "Размер ▼" } else { "Размер" };
                if ui.add_sized([SIZE_W, ROW_H], egui::Button::new(size_txt)).clicked() {
                    self.sort_by_name = false;
                    self.view_dirty = true;
                }
                ui.add_sized([BAR_W, ROW_H], egui::Label::new("Доля"));
                draw_right_text(ui, PCT_W, "%".to_string(), secondary);
                draw_right_text(ui, COUNT_W, "Файлов".to_string(), secondary);
                ui.add_sized([TYPE_W, ROW_H], egui::Label::new("Тип"));
            });
            ui.separator();

            let dir = match self.current_dir() {
                Some(d) => d.clone(),
                None => return,
            };
            let total_size = dir.size.max(1) as f64;

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    let limit = self.view_entries.len().min(self.display_limit);
                    for (i, item) in self.view_entries.iter().enumerate().take(limit) {
                        let is_dir = item.is_dir();
                        let size = item.size();
                        let frac = size as f64 / total_size;
                        let pct = frac * 100.0;
                        let fill_color = if i % 2 == 1 {
                            Color32::from_gray(246)
                        } else {
                            Color32::TRANSPARENT
                        };

                        let full = dir.path.join(item.name());
                        let dir_node_opt: Option<Arc<DirNode>> = match item {
                            Entry::Dir(d) => Some(d.clone()),
                            _ => None,
                        };
                        let enter_req = self.enter_request.clone();
                        let enter_req_menu = enter_req.clone();
                        let dir_node_menu = dir_node_opt.clone();
                        let full_menu = full.clone();

                        egui::Frame::none().fill(fill_color).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = COL_GAP;

                                // Имя
                                let name_resp = if is_dir {
                                    let txt = RichText::new(item.name()).color(Color32::from_rgb(40, 110, 220));
                                    ui.add_sized([NAME_W, ROW_H], egui::Label::new(txt).sense(Sense::click()).truncate())
                                } else {
                                    ui.add_sized([NAME_W, ROW_H], egui::Label::new(item.name()).truncate())
                                };
                                if is_dir && name_resp.clicked() {
                                    if let Some(n) = &dir_node_opt {
                                        *enter_req.lock().unwrap() = Some(n.clone());
                                    }
                                }

                                // Контекстное меню
                                name_resp.context_menu(move |ui| {
                                    if ui.button("Открыть").clicked() {
                                        if let Some(n) = &dir_node_menu {
                                            *enter_req_menu.lock().unwrap() = Some(n.clone());
                                        } else {
                                            let _ = open::that(&full_menu);
                                        }
                                        ui.close_menu();
                                    }
                                    if ui.button("Показать в папке").clicked() {
                                        reveal_path(&full_menu, is_dir);
                                        ui.close_menu();
                                    }
                                    if ui.button("Копировать путь").clicked() {
                                        let s = full_menu.display().to_string();
                                        ui.ctx().output_mut(|o| o.copied_text = s);
                                        ui.close_menu();
                                    }
                                });

                                // Размер
                                draw_right_text(ui, SIZE_W, human_size(size), text_color);

                                // Полоса доли
                                let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(BAR_W, 14.0), Sense::hover());
                                ui.painter().rect_filled(bar_rect, 3.0, Color32::from_gray(225));
                                let fw = (bar_rect.width() * frac as f32).min(bar_rect.width());
                                if fw > 0.5 {
                                    let mut fr = bar_rect;
                                    fr.set_width(fw);
                                    ui.painter().rect_filled(fr, 3.0, Color32::from_rgb(70, 130, 230));
                                }

                                // Процент
                                draw_right_text(ui, PCT_W, format!("{:.1}%", pct), text_color);

                                // Кол-во файлов
                                let cnt = match item {
                                    Entry::Dir(d) => fmt_int(d.count),
                                    _ => "—".to_string(),
                                };
                                draw_right_text(ui, COUNT_W, cnt, secondary);

                                // Тип
                                let kind = if is_dir {
                                    "Папка".to_string()
                                } else {
                                    Path::new(item.name())
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .map(|e| e.to_uppercase())
                                        .unwrap_or_else(|| "ФАЙЛ".into())
                                };
                                ui.add_sized([TYPE_W, ROW_H], egui::Label::new(kind).truncate());
                            });
                        });
                    }

                    if self.view_entries.len() > limit {
                        ui.centered_and_justified(|ui| {
                            ui.label(format!(
                                "… показаны {} крупнейших из {}",
                                limit,
                                self.view_entries.len()
                            ));
                        });
                    }
                });

            // Применяем запрос на переход в папку
            if let Some(n) = self.enter_request.lock().unwrap().take() {
                self.enter_dir(n);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("DiskAnalyzer"),
        ..Default::default()
    };
    eframe::run_native(
        "DiskAnalyzer",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}