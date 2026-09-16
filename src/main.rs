//! # Battery Health Keeper
//!
//! Linux desktop app (eframe/egui) that helps prolong battery lifespan by setting an
//! upper charge limit via kernel sysfs — typically
//! `/sys/class/power_supply/<BAT>/charge_control_end_threshold`.
//!
//! ## What this app does *not* do
//!
//! - It is **not** a power-saving mode (no CPU throttling, screen dimming, etc.).
//! - It does **not** force the battery to discharge on AC; the firmware simply stops
//!   charging once the configured end threshold is reached.
//!
//! ## Architecture (single-file overview)
//!
//! | Section             | Responsibility                                                     |
//! | :--------           | :----------------------------------------------------------------- |
//! | Localization        | `Language`, `find_lang_dir`, `*.lang` key=value files              |
//! | Settings            | JSON load/save (`battery-health-keeper.json`)                      |
//! | Battery / sysfs     | Discover BAT* devices, poll capacity/health, write thresholds      |
//! | UI                  | Custom undecorated window + hardware panel + charge-limit controls |
//! | Desktop integration | Install `.desktop` + icon under `~/.local/share` on startup        |
//!
//! ## Permissions note
//!
//! Writing charge thresholds usually requires a udev rule or elevated rights.
//! The UI reports permission errors in the status message when writes fail.
//!
//! ## Environment
//!
//! - `BATTERY_HEALTH_KEEPER_LANG` — force UI locale (e.g. `en`, `hu`), overrides settings.

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How often live battery fields (capacity, status, thresholds, health) are refreshed.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Settings filename relative to the process working directory.
const SETTINGS_FILE: &str = "battery-health-keeper.json";

// ---------------------------------------------------------------------------
// Localization
// ---------------------------------------------------------------------------

/// Locate the `lang/` directory next to CWD, the executable, or under `target/...`.
///
/// Order: `./lang` → `<exe_dir>/lang` → `<exe_dir>/../../lang` (cargo run layout).
fn find_lang_dir() -> PathBuf {
    let direct = PathBuf::from("lang");
    if direct.is_dir() {
        return direct;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let relative = parent.join("lang");
            if relative.is_dir() {
                return relative;
            }
            let target_root = parent.join("../../lang");
            if target_root.is_dir() {
                return target_root;
            }
        }
    }
    direct
}

/// In-memory UI strings loaded from `lang/<locale>.lang` (`key=value` lines).
#[derive(Clone, Default)]
struct Language {
    /// Locale code matching the filename stem (e.g. `"en"`, `"hu"`).
    locale: String,
    values: HashMap<String, String>,
}

impl Language {
    /// Load locale from `BATTERY_HEALTH_KEEPER_LANG`, defaulting to English.
    fn load() -> Self {
        let locale = std::env::var("BATTERY_HEALTH_KEEPER_LANG").unwrap_or_else(|_| "en".into());
        Self::load_locale(&locale)
    }

    /// Load `lang/<locale>.lang`, falling back to `en.lang`, then empty map.
    fn load_locale(locale: &str) -> Self {
        let lang_dir = find_lang_dir();
        let language_file = lang_dir.join(format!("{locale}.lang"));
        let default_file = lang_dir.join("en.lang");
        let contents = fs::read_to_string(&language_file)
            .or_else(|_| fs::read_to_string(&default_file))
            .or_else(|_| fs::read_to_string(format!("lang/{locale}.lang")))
            .or_else(|_| fs::read_to_string("lang/en.lang"));
        let values = contents
            .ok()
            .map(|contents| {
                contents
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            return None;
                        }
                        let (key, value) = line.split_once('=')?;
                        Some((key.trim().to_owned(), value.trim().to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            locale: locale.into(),
            values,
        }
    }

    /// Look up a translated string; use `fallback` when the key is missing.
    fn text<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.values.get(key).map(String::as_str).unwrap_or(fallback)
    }

    /// List available locale codes from `*.lang` files (sorted, unique).
    fn available_locales() -> Vec<String> {
        let lang_dir = find_lang_dir();
        let mut locales = fs::read_dir(&lang_dir)
            .or_else(|_| fs::read_dir("lang"))
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("lang") {
                    return None;
                }
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>();
        locales.sort();
        locales.dedup();
        locales
    }
}

// ---------------------------------------------------------------------------
// Settings (JSON)
// ---------------------------------------------------------------------------

/// Default charge limit (%) when settings omit `charge_limit` or file is missing.
fn default_charge_limit() -> u8 {
    80
}

/// Default UI language when settings omit `language`.
fn default_language() -> String {
    "en".into()
}

/// Persistent user preferences stored in [`SETTINGS_FILE`].
#[derive(Serialize, Deserialize)]
struct Settings {
    #[serde(default = "default_language")]
    language: String,
    #[serde(default = "default_charge_limit")]
    charge_limit: u8,
}

// ---------------------------------------------------------------------------
// Battery model (Linux sysfs)
// ---------------------------------------------------------------------------

/// One battery from `/sys/class/power_supply` with `type == Battery`.
///
/// Capacity health uses `energy_full` / `energy_full_design` (µWh) when present,
/// otherwise `charge_full` / `charge_full_design` (µAh).
///
/// `cycle_count` is **total charge cycles so far**, not remaining life.
/// Status of hardware and permission support for battery charge thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThresholdSupport {
    /// No charge limit threshold files exist for this battery (unsupported by hardware or kernel driver).
    Unsupported,
    /// Threshold file exists, but cannot be opened for writing (requires root privileges or a udev rule).
    ReadOnly,
    /// Threshold file exists and is writable by the current process.
    Writable,
}

#[derive(Clone)]
struct Battery {
    /// Sysfs directory, e.g. `/sys/class/power_supply/BAT0`.
    root: PathBuf,
    /// Device name (`BAT0`, …).
    name: String,
    /// Current charge percentage (`capacity` file).
    capacity: Option<u8>,
    /// Kernel status string (`Charging`, `Discharging`, …).
    status: String,
    /// Current charge threshold, if exposed.
    current_threshold: Option<u8>,
    /// Threshold support status: Unsupported, ReadOnly, or Writable.
    threshold_support: ThresholdSupport,
    /// Path to end-threshold sysfs file (`charge_control_end_threshold` or `charge_stop_threshold`).
    end_threshold_path: Option<PathBuf>,
    /// Path to start-threshold sysfs file (`charge_control_start_threshold` or `charge_start_threshold`).
    start_threshold_path: Option<PathBuf>,
    /// Backward-compatibility flag (`true` when threshold is writable).
    writable: bool,
    technology: Option<String>,
    manufacturer: Option<String>,
    model_name: Option<String>,
    /// Lifetime charge-cycle counter from the battery/firmware (if available).
    cycle_count: Option<u32>,
    /// Current full-charge capacity in micro-units (µWh or µAh).
    full_capacity: Option<u64>,
    /// Design (new) capacity in the same micro-units.
    design_capacity: Option<u64>,
    /// `"Wh"` when using energy_*, `"Ah"` when using charge_*.
    capacity_unit: &'static str,
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Top-level egui application state.
struct BatteryHealthKeeperApp {
    /// Desired end-of-charge limit (50–100%), applied only when the user clicks Apply/Reset.
    charge_limit: u8,
    language: Language,
    /// Status line shown under the hardware panel.
    message: String,
    batteries: Vec<Battery>,
    icon_texture: Option<egui::TextureHandle>,
    last_poll: Instant,
}

impl BatteryHealthKeeperApp {
    /// Build app state: load settings/language, then discover batteries once.
    fn new() -> Self {
        let saved_settings = load_settings().ok();
        // Priority: env var → saved language → English.
        let language = if let Ok(env_locale) = std::env::var("BATTERY_HEALTH_KEEPER_LANG") {
            Language::load_locale(&env_locale)
        } else if let Some(settings) = saved_settings.as_ref() {
            Language::load_locale(&settings.language)
        } else {
            Language::load()
        };
        let charge_limit = saved_settings
            .map(|s| s.charge_limit)
            .unwrap_or_else(default_charge_limit);

        let mut app = Self {
            charge_limit,
            language: language.clone(),
            message: language.text("status.ready", "Ready").into(),
            batteries: Vec::new(),
            icon_texture: None,
            last_poll: Instant::now(),
        };
        app.refresh_batteries();
        app
    }

    /// Persist language + charge limit to [`SETTINGS_FILE`].
    fn save_settings(&mut self) {
        let result = serde_json::to_string_pretty(&Settings {
            language: self.language.locale.clone(),
            charge_limit: self.charge_limit,
        })
        .map_err(|error| error.to_string())
        .and_then(|contents| fs::write(SETTINGS_FILE, contents).map_err(|error| error.to_string()));

        self.message = match result {
            Ok(()) => format!(
                "{} {SETTINGS_FILE}",
                self.language
                    .text("status.settings_saved", "Settings saved to")
            ),
            Err(error) => format!(
                "{}: {error}",
                self.language
                    .text("status.settings_save_error", "Could not save settings")
            ),
        };
    }

    /// Reload settings from disk into the running UI (does not re-apply hardware limits).
    fn load_settings_into_app(&mut self) {
        if !Path::new(SETTINGS_FILE).exists() {
            self.message = self
                .language
                .text(
                    "status.settings_missing",
                    "No saved settings file exists yet.",
                )
                .into();
            return;
        }
        match load_settings() {
            Ok(settings) => {
                self.charge_limit = settings.charge_limit;
                self.language = Language::load_locale(&settings.language);
                self.message = format!(
                    "{} {SETTINGS_FILE}",
                    self.language
                        .text("status.settings_loaded", "Settings loaded from")
                );
            }
            Err(error) => {
                self.message = format!(
                    "{}: {error}",
                    self.language
                        .text("status.settings_load_error", "Could not load settings")
                );
            }
        }
    }

    /// Re-scan `/sys/class/power_supply` and update the status message.
    fn refresh_batteries(&mut self) {
        self.batteries = discover_batteries();
        self.message = if self.batteries.is_empty() {
            self.language
                .text(
                    "status.no_battery",
                    "No battery found. On Linux, battery control uses /sys/class/power_supply.",
                )
                .into()
        } else if self
            .batteries
            .iter()
            .all(|b| b.threshold_support == ThresholdSupport::Unsupported)
        {
            self.language
                .text(
                    "status.thresholds_unsupported",
                    "Battery found, but charge thresholds are not supported by this hardware or kernel driver.",
                )
                .into()
        } else if self
            .batteries
            .iter()
            .all(|b| b.threshold_support == ThresholdSupport::ReadOnly)
        {
            self.language
                .text(
                    "status.thresholds_permission_denied",
                    "Battery found, but write permission is denied. Run with sudo or configure a udev rule.",
                )
                .into()
        } else {
            format!(
                "{} {}",
                self.batteries.len(),
                self.language
                    .text("status.battery_detected", "battery detected")
            )
        };
    }

    /// Refresh live fields for batteries already discovered (does not add/remove devices).
    fn poll_batteries(&mut self) {
        for battery in &mut self.batteries {
            update_battery_readings(battery);
        }
    }

    /// Write `limit` to every battery that exposes an end-threshold file.
    fn apply_limit(&mut self, limit: u8) {
        if self
            .batteries
            .iter()
            .all(|b| b.threshold_support == ThresholdSupport::Unsupported)
        {
            self.message = self
                .language
                .text(
                    "status.thresholds_unsupported",
                    "Battery found, but charge thresholds are not supported by this hardware or kernel driver.",
                )
                .into();
            return;
        }

        let writable_bats: Vec<_> = self
            .batteries
            .iter()
            .filter(|b| b.threshold_support == ThresholdSupport::Writable)
            .cloned()
            .collect();

        if writable_bats.is_empty() {
            self.message = self
                .language
                .text(
                    "status.thresholds_permission_denied",
                    "Battery found, but write permission is denied. Run with sudo or configure a udev rule.",
                )
                .into();
            return;
        }

        let mut errors = Vec::new();
        for battery in &writable_bats {
            if let Err(e) = apply_charge_limit(battery, limit, &self.language) {
                errors.push(format!("{}: {e}", battery.name));
            }
        }

        self.poll_batteries();

        if errors.is_empty() {
            if limit >= 100 {
                self.message = self
                    .language
                    .text("status.reset_applied", "Charge limit reset to 100%")
                    .into();
            } else {
                self.message = format!(
                    "{}: {limit}%",
                    self.language
                        .text("status.limit_applied", "Charge limit applied")
                );
            }
        } else {
            self.message = format!(
                "{}: {}",
                self.language
                    .text("status.threshold_error", "Could not set thresholds"),
                errors.join("; ")
            );
        }
    }
}

// ---------------------------------------------------------------------------
// UI (eframe)
// ---------------------------------------------------------------------------

impl eframe::App for BatteryHealthKeeperApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep the event loop waking so capacity/status stay reasonably fresh.
        ui.ctx().request_repaint_after(POLL_INTERVAL);

        if self.last_poll.elapsed() >= POLL_INTERVAL {
            self.poll_batteries();
            self.last_poll = Instant::now();
        }

        let language = self.language.clone();
        let available_locales = Language::available_locales();

        // Lazy-load the in-window icon texture once (disk path, else embedded PNG).
        if self.icon_texture.is_none() {
            let bytes_from_disk = fs::read("res/battery-health-keeper.png").ok();
            let icon_bytes: &[u8] = if let Some(ref bytes) = bytes_from_disk {
                bytes.as_slice()
            } else {
                include_bytes!("../res/battery-health-keeper.png")
            };
            if let Ok(img) = image::load_from_memory(icon_bytes) {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    &rgba.into_raw(),
                );
                self.icon_texture = Some(ui.ctx().load_texture(
                    "app_icon",
                    color_image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }

        // --- Custom title bar (native decorations are disabled) ---
        // Close / minimize on the left (macOS-style traffic lights), title centered.
        let title_bar_height = 32.0;
        let (title_rect, _title_response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), title_bar_height),
            egui::Sense::click_and_drag(),
        );

        let title_bg = egui::Color32::from_rgb(40, 40, 40);
        ui.painter().rect_filled(title_rect, 0.0, title_bg);

        // Close
        let btn_size = egui::vec2(20.0, 20.0);
        let btn_y = title_rect.min.y + (title_bar_height - btn_size.y) / 2.0;
        let close_rect =
            egui::Rect::from_min_size(egui::pos2(title_rect.min.x + 8.0, btn_y), btn_size);
        let close_response =
            ui.interact(close_rect, ui.id().with("close_btn"), egui::Sense::click());
        let close_color = if close_response.hovered() {
            egui::Color32::from_rgb(232, 50, 50)
        } else {
            egui::Color32::from_rgb(200, 60, 60)
        };
        ui.painter()
            .circle_filled(close_rect.center(), 8.0, close_color);
        let cx = close_rect.center();
        let d = 3.5;
        ui.painter().line_segment(
            [
                egui::pos2(cx.x - d, cx.y - d),
                egui::pos2(cx.x + d, cx.y + d),
            ],
            egui::Stroke::new(1.5, egui::Color32::WHITE),
        );
        ui.painter().line_segment(
            [
                egui::pos2(cx.x + d, cx.y - d),
                egui::pos2(cx.x - d, cx.y + d),
            ],
            egui::Stroke::new(1.5, egui::Color32::WHITE),
        );
        if close_response.clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Minimize
        let minimize_rect =
            egui::Rect::from_min_size(egui::pos2(close_rect.max.x + 8.0, btn_y), btn_size);
        let minimize_response = ui.interact(
            minimize_rect,
            ui.id().with("minimize_btn"),
            egui::Sense::click(),
        );
        let minimize_color = if minimize_response.hovered() {
            egui::Color32::from_rgb(230, 190, 40)
        } else {
            egui::Color32::from_rgb(200, 170, 50)
        };
        ui.painter()
            .circle_filled(minimize_rect.center(), 8.0, minimize_color);
        let mx = minimize_rect.center();
        ui.painter().line_segment(
            [egui::pos2(mx.x - d, mx.y), egui::pos2(mx.x + d, mx.y)],
            egui::Stroke::new(1.5, egui::Color32::WHITE),
        );
        if minimize_response.clicked() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        ui.painter().text(
            title_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Battery Health Keeper",
            egui::FontId::proportional(14.0),
            egui::Color32::from_rgb(200, 200, 200),
        );

        // Drag the undecorated window from the title bar.
        if _title_response.dragged() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        ui.add_space(4.0);

        // --- Header: icon + title ---
        ui.horizontal(|ui| {
            if let Some(texture) = &self.icon_texture {
                ui.add(egui::Image::new(texture).max_size(egui::vec2(36.0, 36.0)));
            }
            ui.vertical(|ui| {
                ui.heading(language.text("app.title", "Battery Health Keeper"));
                ui.label(language.text("app.subtitle", "Set maximum battery charge limit."));
            });
        });
        ui.add_space(10.0);

        // --- Hardware panel: live status, type, cycles, capacity health ---
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(language.text("ui.hardware", "Hardware"));
                if self.batteries.is_empty() {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        language.text("ui.no_battery", "No battery detected"),
                    );
                } else {
                    for battery in &self.batteries {
                        let state = match battery.threshold_support {
                            ThresholdSupport::Writable => {
                                language.text("ui.thresholds_writable", "thresholds writable")
                            }
                            ThresholdSupport::ReadOnly => {
                                language.text("ui.permission_required", "permission required")
                            }
                            ThresholdSupport::Unsupported => {
                                language.text("ui.thresholds_unsupported", "not supported")
                            }
                        };
                        let status_text = translate_battery_status(&battery.status, &language);
                        let limit_info = if let Some(thresh) = battery.current_threshold {
                            format!(
                                ", {}: {thresh}%",
                                language.text("ui.current_limit", "Active limit")
                            )
                        } else {
                            String::new()
                        };
                        ui.label(format!(
                            "{}: {}% ({status_text}{limit_info}, {state})",
                            battery.name,
                            battery.capacity.unwrap_or(0)
                        ));
                    }
                }
            });
            if !self.batteries.is_empty() {
                for battery in &self.batteries {
                    let tech = battery.technology.as_deref().unwrap_or("?");
                    let mfr = battery.manufacturer.as_deref().unwrap_or("?");
                    let model = battery.model_name.as_deref().unwrap_or("?");
                    let cycles = battery
                        .cycle_count
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| language.text("ui.unavailable", "N/A").into());
                    let name_prefix = if self.batteries.len() > 1 {
                        format!("{} — ", battery.name)
                    } else {
                        String::new()
                    };
                    ui.label(format!(
                        "{name_prefix}{}: {tech} ({mfr}, {model})",
                        language.text("ui.battery_type", "Battery type"),
                    ));
                    ui.label(format!(
                        "{name_prefix}{}: {cycles}",
                        language.text("ui.charging_cycles", "Charge cycles (total)"),
                    ));
                    ui.label(format!(
                        "{name_prefix}{}: {}",
                        language.text("ui.full_capacity", "Full capacity"),
                        format_capacity_info(battery, &language),
                    ));
                }
            }
            ui.label(&self.message);
        });
        ui.add_space(14.0);

        // --- Charge limit slider + presets (Apply still required to write sysfs) ---
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(language.text("ui.charge_limit", "Charge limit (%)"));
                ui.add(
                    egui::Slider::new(&mut self.charge_limit, 50..=100)
                        .suffix("%")
                        .step_by(1.0),
                );
                // Clamp to ensure it never exceeds 100%
                self.charge_limit = self.charge_limit.min(100);
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Presets:");
                if ui
                    .selectable_label(self.charge_limit == 60, "60%")
                    .clicked()
                {
                    self.charge_limit = 60;
                }
                if ui
                    .selectable_label(self.charge_limit == 80, "80%")
                    .clicked()
                {
                    self.charge_limit = 80;
                }
                if ui
                    .selectable_label(self.charge_limit == 100, "100%")
                    .clicked()
                {
                    self.charge_limit = 100;
                }
            });
        });
        ui.add_space(12.0);

        // --- Actions ---
        ui.horizontal(|ui| {
            let apply_btn = egui::Button::new(language.text("button.apply", "Apply limit"));
            if ui.add(apply_btn).clicked() {
                self.apply_limit(self.charge_limit);
            }
            let reset_btn = egui::Button::new(language.text("button.reset", "Reset to 100%"));
            if ui.add(reset_btn).clicked() {
                self.charge_limit = 100;
                self.apply_limit(100);
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button(language.text("button.save", "Save settings"))
                .clicked()
            {
                self.save_settings();
            }
            if ui
                .button(language.text("button.load", "Load settings"))
                .clicked()
            {
                self.load_settings_into_app();
            }
        });

        ui.add_space(8.0);
        ui.label(language.text("ui.linux_note", "Linux note: changing charge thresholds may require elevated permissions and kernel/firmware support."));
        ui.separator();

        // --- Language / version / credits ---
        ui.horizontal(|ui| {
            ui.label(language.text("ui.language", "Language"));
            egui::ComboBox::from_id_salt("language_selector")
                .selected_text(&self.language.locale)
                .show_ui(ui, |ui| {
                    for locale in &available_locales {
                        if ui
                            .selectable_label(self.language.locale == *locale, locale)
                            .clicked()
                        {
                            self.language = Language::load_locale(locale);
                            self.message = format!(
                                "{}: {}",
                                self.language
                                    .text("status.language_changed", "Language changed"),
                                locale
                            );
                            ui.close();
                        }
                    }
                });
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(language.text("ui.version", "Version"));
            ui.label(env!("CARGO_PKG_VERSION"));
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Mihaly Nyilas - dabzse");
            ui.hyperlink("https://dabzse.net");
        });
        ui.vertical(|ui| {
            ui.hyperlink("https://github.com/dabzse/battery-health-keeper-linux");
            ui.hyperlink("https://gitlab.com/dabzse/battery-health-keeper-linux");
        });
    }
}

// ---------------------------------------------------------------------------
// Settings I/O
// ---------------------------------------------------------------------------

/// Read [`SETTINGS_FILE`].
fn load_settings() -> Result<Settings, String> {
    let contents = fs::read_to_string(SETTINGS_FILE).map_err(|error| error.to_string())?;
    serde_json::from_str(&contents).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Sysfs helpers
// ---------------------------------------------------------------------------

/// Find the upper/end charge threshold sysfs path if supported by the driver.
/// Checks modern standard `charge_control_end_threshold`, then legacy `charge_stop_threshold`.
fn find_end_threshold_path(root: &Path) -> Option<PathBuf> {
    let standard = root.join("charge_control_end_threshold");
    if standard.exists() {
        return Some(standard);
    }
    let legacy = root.join("charge_stop_threshold");
    if legacy.exists() {
        return Some(legacy);
    }
    None
}

/// Find the lower/start charge threshold sysfs path if supported by the driver.
/// Checks modern standard `charge_control_start_threshold`, then legacy `charge_start_threshold`.
fn find_start_threshold_path(root: &Path) -> Option<PathBuf> {
    let standard = root.join("charge_control_start_threshold");
    if standard.exists() {
        return Some(standard);
    }
    let legacy = root.join("charge_start_threshold");
    if legacy.exists() {
        return Some(legacy);
    }
    None
}

/// Check whether the threshold node is unsupported, read-only, or writable.
fn check_threshold_support(end_path: Option<&Path>) -> ThresholdSupport {
    let Some(path) = end_path else {
        return ThresholdSupport::Unsupported;
    };
    if !path.exists() {
        return ThresholdSupport::Unsupported;
    }
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => ThresholdSupport::Writable,
        Err(_) => ThresholdSupport::ReadOnly,
    }
}

/// Enumerate batteries under `/sys/class/power_supply` (entries with `type=Battery`).
fn discover_batteries() -> Vec<Battery> {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let root = entry.path();
            let type_name = read_trimmed(root.join("type"))?;
            if type_name != "Battery" {
                return None;
            }
            let end_threshold_path = find_end_threshold_path(&root);
            let start_threshold_path = find_start_threshold_path(&root);
            let threshold_support = check_threshold_support(end_threshold_path.as_deref());
            let writable = threshold_support == ThresholdSupport::Writable;
            let mut battery = Battery {
                name: entry.file_name().to_string_lossy().into_owned(),
                root,
                capacity: None,
                status: "Unknown".into(),
                current_threshold: None,
                threshold_support,
                end_threshold_path,
                start_threshold_path,
                writable,
                technology: None,
                manufacturer: None,
                model_name: None,
                cycle_count: None,
                full_capacity: None,
                design_capacity: None,
                capacity_unit: "Wh",
            };
            update_battery_readings(&mut battery);
            Some(battery)
        })
        .collect()
}

/// Refresh dynamic and semi-static fields from this battery's sysfs directory.
fn update_battery_readings(battery: &mut Battery) {
    if let Some(cap) =
        read_trimmed(battery.root.join("capacity")).and_then(|v| v.parse::<u8>().ok())
    {
        battery.capacity = Some(cap);
    }
    if let Some(status) = read_trimmed(battery.root.join("status")) {
        battery.status = status;
    }
    if battery.end_threshold_path.is_none() {
        battery.end_threshold_path = find_end_threshold_path(&battery.root);
    }
    if battery.start_threshold_path.is_none() {
        battery.start_threshold_path = find_start_threshold_path(&battery.root);
    }
    battery.threshold_support = check_threshold_support(battery.end_threshold_path.as_deref());
    battery.writable = battery.threshold_support == ThresholdSupport::Writable;

    battery.current_threshold = battery
        .end_threshold_path
        .as_ref()
        .and_then(read_trimmed)
        .and_then(|v| v.parse::<u8>().ok());
    battery.technology = read_trimmed(battery.root.join("technology"));
    battery.manufacturer = read_trimmed(battery.root.join("manufacturer"));
    battery.model_name = read_trimmed(battery.root.join("model_name"));
    battery.cycle_count =
        read_trimmed(battery.root.join("cycle_count")).and_then(|v| v.parse().ok());

    // Prefer energy_* (µWh); fall back to charge_* (µAh) on some laptops.
    let energy_full = read_trimmed(battery.root.join("energy_full")).and_then(|v| v.parse().ok());
    let energy_design =
        read_trimmed(battery.root.join("energy_full_design")).and_then(|v| v.parse().ok());
    let charge_full = read_trimmed(battery.root.join("charge_full")).and_then(|v| v.parse().ok());
    let charge_design =
        read_trimmed(battery.root.join("charge_full_design")).and_then(|v| v.parse().ok());

    if energy_full.is_some() || energy_design.is_some() {
        battery.full_capacity = energy_full;
        battery.design_capacity = energy_design;
        battery.capacity_unit = "Wh";
    } else {
        battery.full_capacity = charge_full;
        battery.design_capacity = charge_design;
        battery.capacity_unit = "Ah";
    }
}

/// Format sysfs micro-units (µWh / µAh) as a one-decimal Wh / Ah string.
fn format_micro_capacity(micro: u64, unit: &str) -> String {
    format!("{:.1} {unit}", micro as f64 / 1_000_000.0)
}

/// Wear estimate: current full capacity as a percentage of design capacity.
fn battery_health_percent(full: u64, design: u64) -> Option<u8> {
    if design == 0 {
        return None;
    }
    Some(((full * 100) / design).min(100) as u8)
}

/// Human-readable capacity line, e.g. `57.1 Wh / 90.1 Wh design (63%)`.
fn format_capacity_info(battery: &Battery, language: &Language) -> String {
    match (battery.full_capacity, battery.design_capacity) {
        (Some(full), Some(design)) => {
            let full_s = format_micro_capacity(full, battery.capacity_unit);
            let design_s = format_micro_capacity(design, battery.capacity_unit);
            if let Some(health) = battery_health_percent(full, design) {
                format!(
                    "{full_s} / {design_s} {} ({health}%)",
                    language.text("ui.design_capacity", "design")
                )
            } else {
                format!(
                    "{full_s} / {design_s} {}",
                    language.text("ui.design_capacity", "design")
                )
            }
        }
        (Some(full), None) => format_micro_capacity(full, battery.capacity_unit),
        (None, Some(design)) => format!(
            "{} {}",
            format_micro_capacity(design, battery.capacity_unit),
            language.text("ui.design_capacity", "design")
        ),
        (None, None) => language.text("ui.unavailable", "N/A").into(),
    }
}

/// Read a sysfs text file and trim whitespace/newlines.
fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

/// Write one threshold percentage to a sysfs node.
fn write_single_threshold(path: &Path, value: u8, language: &Language) -> Result<(), String> {
    fs::write(path, value.to_string()).map_err(|error| format_threshold_error(error, language))
}

/// Apply an end-of-charge limit; lower start threshold first when needed (ThinkPad-style).
fn apply_charge_limit(battery: &Battery, limit: u8, language: &Language) -> Result<(), String> {
    let Some(ref end_path) = battery.end_threshold_path else {
        return Err(language
            .text(
                "status.thresholds_unsupported",
                "Battery found, but charge thresholds are not supported by this hardware or kernel driver.",
            )
            .into());
    };

    if !end_path.exists() {
        return Err(language
            .text(
                "status.thresholds_unsupported",
                "Battery found, but charge thresholds are not supported by this hardware or kernel driver.",
            )
            .into());
    }

    // Kernel requires start < end; otherwise writing end returns EINVAL.
    if let Some(ref start_path) = battery.start_threshold_path {
        if start_path.exists() {
            let current_start: Option<u8> = read_trimmed(start_path).and_then(|s| s.parse().ok());
            if let Some(start) = current_start {
                if start >= limit {
                    let new_start = limit.saturating_sub(5);
                    let _ = write_single_threshold(start_path, new_start, language);
                }
            }
        }
    }

    write_single_threshold(end_path, limit, language)
}

/// Map I/O errors to localized messages (especially permission denied).
fn format_threshold_error(error: std::io::Error, language: &Language) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        language
            .text(
                "status.permission_denied",
                "Permission denied. Grant access to the battery threshold files or run the app with the required permissions.",
            )
            .into()
    } else {
        error.to_string()
    }
}

// ---------------------------------------------------------------------------
// Desktop entry / icons
// ---------------------------------------------------------------------------

/// Install/overwrite a FreeDesktop `.desktop` file and 128×128 icon under XDG data dirs.
///
/// Currently runs on every launch (name is historical); failures are ignored so the
/// GUI still starts on locked-down or non-standard environments.
fn install_desktop_entry_if_needed() {
    let Some(data_dir) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };

    let apps_dir = data_dir.join("applications");
    let icons_dir = data_dir.join("icons/hicolor/128x128/apps");

    let _ = fs::create_dir_all(&apps_dir);
    let _ = fs::create_dir_all(&icons_dir);

    let icon_path = icons_dir.join("battery-health-keeper-linux.png");
    let bytes_from_disk = fs::read("res/battery-health-keeper.png").ok();
    let icon_bytes: &[u8] = if let Some(ref bytes) = bytes_from_disk {
        bytes.as_slice()
    } else {
        include_bytes!("../res/battery-health-keeper.png")
    };

    if let Ok(img) = image::load_from_memory(icon_bytes) {
        let resized = img.resize_exact(128, 128, image::imageops::FilterType::Lanczos3);
        let _ = resized.save(&icon_path);
    }

    let exec_path = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "battery-health-keeper-linux".into());

    let desktop_content = format!(
        "
            [Desktop Entry]\n\
            Name=Battery Health Keeper\n\
            Comment=A simple battery health keeper for Linux\n\
            Exec=\"{exec_path}\"\n\
            Icon=battery-health-keeper-linux\n\
            Terminal=false\n\
            Type=Application\n\
            Categories=Utility;Settings;HardwareSettings;\n\
            StartupWMClass=battery-health-keeper-linux\n
        "
    );

    let desktop_file = apps_dir.join("battery-health-keeper-linux.desktop");
    let _ = fs::write(desktop_file, desktop_content);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    install_desktop_entry_if_needed();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("battery-health-keeper-linux")
            .with_title("Battery Health Keeper")
            .with_inner_size([500.0, 530.0])
            .with_resizable(false)
            // Custom title bar is drawn in `ui`; keep OS chrome off.
            .with_decorations(false)
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Battery Health Keeper",
        native_options,
        Box::new(|_cc| Ok(Box::new(BatteryHealthKeeperApp::new()))),
    )
}

/// Window/taskbar icon: prefer `res/…png` on disk, else the PNG embedded at compile time.
fn app_icon() -> egui::IconData {
    let bytes_from_disk = fs::read("res/battery-health-keeper.png").ok();
    let icon_bytes: &[u8] = if let Some(ref bytes) = bytes_from_disk {
        bytes.as_slice()
    } else {
        include_bytes!("../res/battery-health-keeper.png")
    };

    if let Ok(img) = image::load_from_memory(icon_bytes) {
        let resized = img.resize_exact(128, 128, image::imageops::FilterType::Lanczos3);
        let rgba = resized.to_rgba8();
        let (width, height) = rgba.dimensions();
        egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }
    } else {
        fallback_icon()
    }
}

/// Tiny procedural battery icon used only if the PNG cannot be decoded.
fn fallback_icon() -> egui::IconData {
    const SIZE: usize = 32;
    let mut rgba = vec![0; SIZE * SIZE * 4];
    let mut pixel = |x: usize, y: usize, color: [u8; 4]| {
        if x < SIZE && y < SIZE {
            let offset = (y * SIZE + x) * 4;
            rgba[offset..offset + 4].copy_from_slice(&color);
        }
    };
    let outline = [44, 62, 72, 255];
    let green = [55, 190, 125, 255];
    for y in 7..26 {
        for x in 5..27 {
            if x == 5 || x == 26 || y == 7 || y == 25 {
                pixel(x, y, outline);
            }
        }
    }
    for y in 11..23 {
        for x in 9..23 {
            pixel(x, y, green);
        }
    }
    for y in 13..20 {
        for x in 27..29 {
            pixel(x, y, outline);
        }
    }
    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// Translate kernel battery `status` strings using `battery.status.*` / `status.*` keys.
fn translate_battery_status<'a>(status: &'a str, language: &'a Language) -> &'a str {
    match status.to_ascii_lowercase().as_str() {
        "charging" => language.text(
            "battery.status.charging",
            language.text("status.charging", "charging"),
        ),
        "discharging" => language.text(
            "battery.status.discharging",
            language.text("status.discharging", "discharging"),
        ),
        "not charging" => language.text("battery.status.not_charging", "not charging"),
        "full" => language.text("battery.status.full", "full"),
        "unknown" => language.text("battery.status.unknown", "unknown"),
        _ => status,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_loading() {
        let en = Language::load_locale("en");
        assert_eq!(en.text("app.title", ""), "Battery Health Keeper");
        assert_eq!(en.text("button.apply", ""), "Apply limit");
        assert_eq!(translate_battery_status("Discharging", &en), "discharging");

        let hu = Language::load_locale("hu");
        assert_eq!(hu.text("app.title", ""), "Akkumulátor egészségmegőrző");
        assert_eq!(hu.text("button.apply", ""), "Korlát alkalmazása");
        assert_eq!(translate_battery_status("Discharging", &hu), "merülés");
        assert_eq!(
            hu.text("ui.thresholds_writable", ""),
            "írható küszöbértékek"
        );
        assert_eq!(en.text("ui.thresholds_unsupported", ""), "not supported");
        assert_eq!(hu.text("ui.thresholds_unsupported", ""), "nem támogatott");
        assert_eq!(en.text("ui.permission_required", ""), "permission required");
        assert_eq!(hu.text("ui.permission_required", ""), "engedély szükséges");
    }

    #[test]
    fn test_settings_serde_roundtrip() {
        let json = serde_json::to_string(&Settings {
            language: "hu".into(),
            charge_limit: 85,
        })
        .unwrap();
        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.charge_limit, 85);
        assert_eq!(loaded.language, "hu");

        // Backward compatibility: files with "cycles" array (ignored)
        let legacy_json = r#"{
            "language": "hu",
            "cycles": [
                { "enabled": true, "charge_to": 89, "discharge_to": 15 },
                { "enabled": false, "charge_to": null, "discharge_to": null }
            ]
        }"#;
        let legacy_loaded: Settings = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(legacy_loaded.language, "hu");
        assert_eq!(legacy_loaded.charge_limit, 80); // default
    }

    #[test]
    fn test_app_icon_loads() {
        let icon = app_icon();
        assert!(icon.width > 0);
        assert!(icon.height > 0);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn test_missing_locale_falls_back_to_english() {
        let lang = Language::load_locale("xx_nonexistent");
        // Should fall back to "en" content
        assert_eq!(lang.locale, "xx_nonexistent");
        assert_eq!(lang.text("app.title", "FALLBACK"), "Battery Health Keeper");
        // The fallback parameter is returned for truly missing keys
        assert_eq!(lang.text("totally.bogus.key", "FALLBACK"), "FALLBACK");
    }

    #[test]
    fn test_translate_battery_status_all_variants() {
        let en = Language::load_locale("en");

        assert_eq!(translate_battery_status("Charging", &en), "charging");
        assert_eq!(translate_battery_status("Discharging", &en), "discharging");
        assert_eq!(
            translate_battery_status("Not charging", &en),
            "not charging"
        );
        assert_eq!(translate_battery_status("Full", &en), "full");
        assert_eq!(translate_battery_status("Unknown", &en), "unknown");

        // Case insensitivity
        assert_eq!(translate_battery_status("CHARGING", &en), "charging");
        assert_eq!(translate_battery_status("full", &en), "full");

        // Unrecognised status is returned as-is
        assert_eq!(
            translate_battery_status("SomeWeirdValue", &en),
            "SomeWeirdValue"
        );
    }

    #[test]
    fn test_fallback_icon_valid() {
        let icon = fallback_icon();
        assert_eq!(icon.width, 32);
        assert_eq!(icon.height, 32);
        assert_eq!(icon.rgba.len(), (32 * 32 * 4) as usize);
    }

    #[test]
    fn test_settings_defaults_from_empty_json() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.charge_limit, 80);
        assert_eq!(settings.language, "en");
    }

    #[test]
    fn test_capacity_health_and_format() {
        assert_eq!(battery_health_percent(57_150_000, 90_060_000), Some(63));
        assert_eq!(battery_health_percent(90_060_000, 90_060_000), Some(100));
        assert_eq!(battery_health_percent(1, 0), None);
        assert_eq!(format_micro_capacity(57_150_000, "Wh"), "57.1 Wh");
        assert_eq!(format_micro_capacity(90_060_000, "Wh"), "90.1 Wh");
    }

    #[test]
    fn test_threshold_support_check() {
        assert_eq!(check_threshold_support(None), ThresholdSupport::Unsupported);

        let non_existent = Path::new("/nonexistent/sysfs/battery/threshold");
        assert_eq!(
            check_threshold_support(Some(non_existent)),
            ThresholdSupport::Unsupported
        );

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!(
            "test_battery_threshold_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&test_file, "80").unwrap();
        assert_eq!(
            check_threshold_support(Some(&test_file)),
            ThresholdSupport::Writable
        );
        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn test_find_threshold_paths_fallback() {
        let temp_dir = std::env::temp_dir().join(format!(
            "test_bat_sysfs_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        // Neither exists
        assert!(find_end_threshold_path(&temp_dir).is_none());
        assert!(find_start_threshold_path(&temp_dir).is_none());

        // Legacy exists
        let legacy_end = temp_dir.join("charge_stop_threshold");
        let legacy_start = temp_dir.join("charge_start_threshold");
        fs::write(&legacy_end, "80").unwrap();
        fs::write(&legacy_start, "40").unwrap();

        assert_eq!(find_end_threshold_path(&temp_dir), Some(legacy_end.clone()));
        assert_eq!(
            find_start_threshold_path(&temp_dir),
            Some(legacy_start.clone())
        );

        // Standard overrides legacy if both exist
        let standard_end = temp_dir.join("charge_control_end_threshold");
        let standard_start = temp_dir.join("charge_control_start_threshold");
        fs::write(&standard_end, "80").unwrap();
        fs::write(&standard_start, "40").unwrap();

        assert_eq!(find_end_threshold_path(&temp_dir), Some(standard_end));
        assert_eq!(find_start_threshold_path(&temp_dir), Some(standard_start));

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
