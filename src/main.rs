use eframe::egui::{self, Color32, RichText};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

#[derive(Clone)]
struct DirNode {
    name: String,
    path: PathBuf,
    size: u64,
    count: u64,
    children: Vec<Entry>,
    has_error: bool,
}

// ---------------------------------------------------------------------------
// Быстрый параллельный сканер
// ---------------------------------------------------------------------------

fn scan_tree(path: &Path, progress: &Arc<Mutex<(u64, u64)>>) -> Result<DirNode, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut dir_size = 0;
    let mut dir_count = 0;
    let mut has_error = false;
    let mut entries = Vec::new();

    let read_dir = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            return Ok(DirNode {
                name,
                path: path.to_path_buf(),
                size: 0,
                count: 0,
                children: vec![],
                has_error: true,
            });
        }
    };

    // Собираем пути для параллельной обработки
    let mut paths = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(e) => paths.push(e.path()),
            Err(_) => has_error = true,
        }
    }

    // Параллельно сканируем подпапки с помощью Rayon
    let sub_results: Vec<_> = paths
        .par_iter()
        .filter_map(|p| {
            let meta = p.symlink_metadata().ok()?;
            if meta.is_dir() {
                Some((p.clone(), scan_tree(p, progress).ok()?))
            } else {
                let size = meta.len();
                // Атомарно обновляем прогресс
                let mut prog = progress.lock().unwrap();
                prog.0 += 1;
                prog.1 += size;
                None
            }
        })
        .collect();

    // Формируем список детей (Rayon сохраняет порядок относительно par_iter)
    let mut sub_idx = 0;
    for p in &paths {
        if let Ok(meta) = p.symlink_metadata() {
            let fname = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            if meta.is_dir() {
                if let Some((_, node)) = sub_results.get(sub_idx) {
                    if p == &node.path {
                        dir_size += node.size;
                        dir_count += node.count;
                        has_error |= node.has_error;
                        entries.push(Entry::Dir(Arc::new(node.clone())));
                        sub_idx += 1;
                    }
                }
            } else {
                let size = meta.len();
                dir_size += size;
                dir_count += 1;
                entries.push(Entry::File { name: fname, size });
            }
        }
    }

    // Сортируем по убыванию размера
    entries.sort_by(|a, b| b.size().cmp(&a.size()));

    Ok(DirNode {
        name,
        path: path.to_path_buf(),
        size: dir_size,
        count: dir_count,
        children: entries,
        has_error,
    })
}

// ---------------------------------------------------------------------------
// Утилиты
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

// ---------------------------------------------------------------------------
// Состояние приложения
// ---------------------------------------------------------------------------

struct App {
    root_node: Option<Arc<DirNode>>,
    stack: Vec<Arc<DirNode>>,
    sort_by_name: bool,
    scanning: bool,
    scan_progress: Arc<Mutex<(u64, u64)>>,
    scan_start: Option<Instant>,
    scan_error: Option<String>,
    display_limit: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            root_node: None,
            stack: Vec::new(),
            sort_by_name: false,
            scanning: false,
            scan_progress: Arc::new(Mutex::new((0, 0))),
            scan_start: None,
            scan_error: None,
            display_limit: 5000,
        }
    }
}

impl App {
    fn current_dir(&self) -> Option<&Arc<DirNode>> {
        self.stack.last()
    }

    fn go_up(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    fn jump_to(&mut self, idx: usize) {
        if idx < self.stack.len() {
            self.stack.truncate(idx + 1);
        }
    }

    fn enter_dir(&mut self, node: Arc<DirNode>) {
        self.stack.push(node);
    }

    fn start_scan(&mut self, path: PathBuf) {
        self.scanning = true;
        self.scan_error = None;
        self.scan_start = Some(Instant::now());
        *self.scan_progress.lock().unwrap() = (0, 0);

        let progress = self.scan_progress.clone();
        
        // Запускаем сканирование в фоновом потоке
        std::thread::spawn(move || {
            match scan_tree(&path, &progress) {
                Ok(node) => {
                    // Используем канал или глобальное состояние? 
                    // В eframe проще передать результат через Arc<Mutex<Option>>
                    // Но мы можем использовать custom event или просто поллить.
                    // Для простоты сохраним во временный файл? Нет.
                    // Сделаем через статическую переменную для примера, 
                    // но лучше добавить поле в App.
                    
                    // Мы передадим результат через тот же progress mutex,
                    // заменив его на более сложную структуру, ИЛИ
                    // просто добавим поле в App.
                    // Так как egui immediate mode, добавим Arc<Mutex<Option<DirNode>>>
                }
                Err(e) => {}
            }
        });
    }
}

// Чтобы передать результат из потока, расширим App
struct AppState {
    result: Arc<Mutex<Option<Result<DirNode, String>>>>,
}

impl App {
    fn trigger_scan(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.scanning = true;
            self.scan_error = None;
            self.scan_start = Some(Instant::now());
            *self.scan_progress.lock().unwrap() = (0, 0);
            
            let progress = self.scan_progress.clone();
            let result_store = self.result.clone();
            let ctx_clone = ctx.clone();

            std::thread::spawn(move || {
                let res = scan_tree(&path, &progress);
                *result_store.lock().unwrap() = Some(res);
                // Будим UI поток, чтобы он заметил результат
                ctx_clone.request_repaint();
            });
        }
    }
}

// Переопределим Default с учетом нового поля
impl App {
    fn new() -> Self {
        Self {
            root_node: None,
            stack: Vec::new(),
            sort_by_name: false,
            scanning: false,
            scan_progress: Arc::new(Mutex::new((0, 0))),
            scan_start: None,
            scan_error: None,
            display_limit: 5000,
            result: Arc::new(Mutex::new(None)),
        }
    }
}

// Удалим старый Default и используем new()

// ---------------------------------------------------------------------------
// Отрисовка GUI (Immediate Mode)
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Проверяем, завершилось ли сканирование
        if self.scanning {
            let mut res_lock = self.result.lock().unwrap();
            if let Some(res) = res_lock.take() {
                self.scanning = false;
                match res {
                    Ok(node) => {
                        let arc_node = Arc::new(node);
                        self.root_node = Some(arc_node.clone());
                        self.stack.clear();
                        self.stack.push(arc_node);
                    }
                    Err(e) => {
                        self.scan_error = Some(e);
                    }
                }
            } else {
                // Просим перерисовать через 100мс для обновления прогресса
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        // Стилизация
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        ctx.set_style(style);

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("📁 Открыть папку…").clicked() && !self.scanning {
                    self.trigger_scan(ctx);
                }
                
                let can_up = self.stack.len() > 1 && !self.scanning;
                if ui.add_enabled(can_up, egui::Button::new("⬆ Вверх")).clicked() {
                    self.go_up();
                }

                if self.scanning {
                    ui.spinner();
                    let (files, bytes) = *self.scan_progress.lock().unwrap();
                    ui.label(format!(
                        "Сканирование… файлов: {} • {}",
                        fmt_int(files),
                        human_size(bytes)
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
                for (i, node) in self.stack.iter().enumerate() {
                    if i > 0 {
                        ui.label("›");
                    }
                    let is_last = i == self.stack.len() - 1;
                    let text = if node.name.len() > 25 {
                        format!("{}…", &node.name[..24])
                    } else {
                        node.name.clone()
                    };
                    
                    if is_last {
                        ui.label(RichText::new(text).strong());
                    } else {
                        if ui.link(text).clicked() {
                            self.jump_to(i);
                        }
                    }
                }
            });
            ui.separator();
        });

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

        // Центральная панель со списком
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.root_node.is_none() && !self.scanning {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("DiskAnalyzer");
                    ui.label("Нажмите «Открыть папку…» для начала анализа");
                });
                return;
            }

            let dir = match self.current_dir() {
                Some(d) => d.clone(),
                None => return,
            };

            // Заголовки таблицы
            ui.horizontal(|ui| {
                let name_btn = ui.selectable_label(!self.sort_by_name, "Имя ▼"); // упрощенно
                if ui.button(if self.sort_by_name { "Имя ▲" } else { "Имя" }).clicked() {
                    self.sort_by_name = true;
                }
                if ui.button(if !self.sort_by_name { "Размер ▼" } else { "Размер" }).clicked() {
                    self.sort_by_name = false;
                }
                ui.label("Доля");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("Тип      ");
                    ui.label("Файлов   ");
                });
            });
            ui.separator();

            // Скроллируемый список
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                let total_size = dir.size.max(1) as f64;
                
                // Сортировка
                let mut children = dir.children.clone();
                if self.sort_by_name {
                    children.sort_by(|a, b| {
                        a.is_dir().cmp(&b.is_dir()).reverse()
                            .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
                    });
                } else {
                    children.sort_by(|a, b| b.size().cmp(&a.size()));
                }

                let limit = children.len().min(self.display_limit);
                
                for item in children.iter().take(limit) {
                    let is_dir = item.is_dir();
                    let size = item.size();
                    let frac = size as f64 / total_size;
                    let pct = frac * 100.0;

                    ui.horizontal(|ui| {
                        // Имя (кликабельное для папок)
                        let name_text = item.name();
                        let name_widget = if is_dir {
                            let resp = ui.link(RichText::new(name_text).color(Color32::from_rgb(30, 100, 220)));
                            if resp.clicked() {
                                if let Entry::Dir(node) = item {
                                    self.enter_dir(node.clone());
                                }
                            }
                            if resp.double_clicked() {
                                if let Entry::Dir(node) = item {
                                    self.enter_dir(node.clone());
                                }
                            }
                            resp
                        } else {
                            ui.label(name_text)
                        };

                        // Контекстное меню
                        name_widget.context_menu(|ui| {
                            if ui.button("Открыть").clicked() {
                                let _ = open::that(&item.name()); // упрощение
                                ui.close_menu();
                            }
                            if ui.button("Показать в папке").clicked() {
                                let path = dir.path.join(item.name());
                                #[cfg(target_os = "windows")]
                                {
                                    std::process::Command::new("explorer")
                                        .args(["/select,", path.to_str().unwrap_or("")])
                                        .spawn().ok();
                                }
                                #[cfg(not(target_os = "windows"))]
                                {
                                    let _ = open::that(path.parent().unwrap_or(&path));
                                }
                                ui.close_menu();
                            }
                            if ui.button("Копировать путь").clicked() {
                                let path = dir.path.join(item.name());
                                ui.output_mut(|o| o.copied_text = path.display().to_string());
                                ui.close_menu();
                            }
                        });

                        // Размер
                        ui.label(RichText::new(human_size(size)).monospace());

                        // Полоса доли (прогресс-бар)
                        let bar_width = 150.0;
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_width, 14.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, Color32::from_gray(230));
                        let fill_width = (rect.width() * frac as f32).min(rect.width());
                        if fill_width > 0.0 {
                            let mut fill_rect = rect;
                            fill_rect.set_width(fill_width);
                            ui.painter().rect_filled(fill_rect, 2.0, Color32::from_rgb(70, 130, 230));
                        }
                        ui.label(format!("{:.1}%", pct));

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Тип
                            let kind = if is_dir {
                                "Папка".to_string()
                            } else {
                                Path::new(item.name())
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .map(|e| e.to_uppercase())
                                    .unwrap_or_else(|| "ФАЙЛ".to_string())
                            };
                            ui.label(kind);

                            // Кол-во файлов
                            if let Entry::Dir(d) = item {
                                ui.label(fmt_int(d.count));
                            } else {
                                ui.label("—");
                            }
                        });
                    });
                    
                    // Разделитель строк (зебра)
                    ui.separator();
                }

                if children.len() > limit {
                    ui.centered_and_justified(|ui| {
                        ui.label(format!("… показаны {} крупнейших из {}", limit, children.len()));
                    });
                }
            });
        });

        // Горячие клавиши
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::CTRL, egui::Key::O) {
                if !self.scanning {
                    self.trigger_scan(ctx);
                }
            }
            if i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::ArrowLeft) && i.modifiers.alt {
                self.go_up();
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