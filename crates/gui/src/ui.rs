//! The settings UI: one row per `ShmHeader` setting, grouped and tabbed the way
//! upstream's Qt GUI does (Model, Motion, Composition, Status) — used as a checklist
//! of what has to exist, not as layout code to port; a per-field `QCheckBox`/
//! `QSpinBox` binder has no logic worth transliterating either way.

use std::sync::atomic::Ordering;

use neuralforge_protocol::enums::{colour_mode, downscaler, mvec_quality, mvec_scale_mode, reversible_mode};
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::shm::{bind_bool, bind_float, bind_u32, Shm};

fn spin_row(title: &str, subtitle: &str, value: f32, lower: f64, upper: f64, step: f64, setter: impl Fn(f32) + 'static) -> adw::SpinRow {
    let adjustment = gtk4::Adjustment::new(value as f64, lower, upper, step, step * 10.0, 0.0);
    let row = adw::SpinRow::new(Some(&adjustment), step, 2);
    row.set_title(title);
    row.set_subtitle(subtitle);
    adjustment.connect_value_changed(move |adj| setter(adj.value() as f32));
    row
}

fn switch_row(title: &str, subtitle: &str, active: bool, setter: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_active(active);
    row.connect_active_notify(move |row| setter(row.is_active()));
    row
}

fn combo_row(title: &str, options: &[&str], selected: u32, setter: impl Fn(u32) + 'static) -> adw::ComboRow {
    let model = gtk4::StringList::new(options);
    let row = adw::ComboRow::new();
    row.set_title(title);
    row.set_model(Some(&model));
    row.set_selected(selected.min(options.len() as u32 - 1));
    row.connect_selected_notify(move |row| setter(row.selected()));
    row
}

/// GDK on Linux X11/Wayland uses XKB hardware codes (evdev + 8).
fn evdev_keycode(hardware: u32) -> Option<u32> {
    hardware.checked_sub(8).filter(|&code| code > 0 && code <= 767)
}

fn hotkey_row(initial: u32, setter: impl Fn(u32) + 'static) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Toggle key")
        .subtitle("Save a single key binding; in-game hotkey polling is not available yet") .build();
    let label = |code| if code == 0 { "Unbound".to_owned() } else { format!("Key {code}") };
    let button = gtk4::Button::with_label(&label(initial));
    button.set_valign(gtk4::Align::Center);
    let clear = gtk4::Button::with_label("Clear");
    clear.set_valign(gtk4::Align::Center);
    let value = std::rc::Rc::new(std::cell::Cell::new(initial));
    let capturing = std::rc::Rc::new(std::cell::Cell::new(false));
    let setter = std::rc::Rc::new(setter);
    let controller = gtk4::EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let capturing = capturing.clone();
        button.connect_clicked(move |button| {
            capturing.set(true);
            button.set_label("Press a key (Esc cancels)");
            button.grab_focus();
        });
    }
    {
        let button = button.downgrade();
        let capturing = capturing.clone();
        let value = value.clone();
        let setter = setter.clone();
        controller.connect_key_pressed(move |_, key, hardware, _| {
            if !capturing.get() { return glib::Propagation::Proceed; }
            if let Some(button) = button.upgrade() {
                if key != gtk4::gdk::Key::Escape {
                    if let Some(code) = evdev_keycode(hardware) {
                        setter(code);
                        value.set(code);
                    }
                }
                capturing.set(false);
                button.set_label(&label(value.get()));
            }
            glib::Propagation::Stop
        });
    }
    {
        let capturing = capturing.clone();
        let value = value.clone();
        button.connect_has_focus_notify(move |button| {
            if !button.has_focus() && capturing.replace(false) {
                button.set_label(&label(value.get()));
            }
        });
    }
    {
        let button = button.clone();
        clear.connect_clicked(move |_| {
            capturing.set(false);
            value.set(0);
            setter(0);
            button.set_label("Unbound");
        });
    }
    button.add_controller(controller);
    row.add_suffix(&button);
    row.add_suffix(&clear);
    row
}

pub fn build_ui(app: &adw::Application) {
    let Some(shm) = Shm::open() else {
        build_error_window(app);
        return;
    };
    let shm = shm.0;

    // --- Model -------------------------------------------------------------------
    let model_group = adw::PreferencesGroup::new();
    model_group.set_title("Model");

    let (enabled, set_enabled) = bind_bool(&shm, Some("enabled"), |h| &h.enabled);
    model_group.add(&switch_row("Neural rendering", "Off keeps the layer running but presents the original frame", enabled, set_enabled));

    let (style, set_style) = bind_u32(&shm, Some("style"), |h| &h.style);
    model_group.add(&combo_row("Style", &["Default", "Natural", "Cinematic"], style, set_style));

    let (preset, set_preset) = bind_u32(&shm, Some("preset"), |h| &h.preset);
    model_group.add(&spin_row("Preset", "0-3, model-defined presets", preset as f32, 0.0, 3.0, 1.0, move |v| set_preset(v as u32)));

    let (intensity, set_intensity) = bind_float(&shm, Some("intensity"), |h| &h.intensity_bits);
    model_group.add(&spin_row("Intensity", "", intensity, 0.0, 2.0, 0.05, set_intensity));

    let (local_tone, set_local_tone) = bind_float(&shm, Some("local_tone"), |h| &h.local_tone_bits);
    model_group.add(&spin_row("Local tone", "", local_tone, 0.0, 2.0, 0.05, set_local_tone));

    let (local_structure, set_local_structure) = bind_float(&shm, Some("local_structure"), |h| &h.local_structure_bits);
    model_group.add(&spin_row("Local structure", "", local_structure, 0.0, 2.0, 0.05, set_local_structure));

    let (skin_structure, set_skin_structure) = bind_float(&shm, Some("skin_structure"), |h| &h.skin_structure_bits);
    model_group.add(&spin_row("Skin structure", "-1 follows local structure", skin_structure, -1.0, 2.0, 0.05, set_skin_structure));

    let (sharpness, set_sharpness) = bind_float(&shm, Some("sharpness"), |h| &h.sharpness_bits);
    model_group.add(&spin_row("Sharpness", "", sharpness, 0.0, 1.0, 0.05, set_sharpness));

    let (auto_mask, set_auto_mask) = bind_bool(&shm, Some("auto_mask"), |h| &h.auto_mask);
    model_group.add(&switch_row("Auto mask", "Automatic skin/detail masking", auto_mask, set_auto_mask));

    let (passes, set_passes) = bind_u32(&shm, Some("passes"), |h| &h.passes);
    model_group.add(&spin_row("Passes", "How many times the model runs over one frame", passes as f32, 1.0, 30.0, 1.0, move |v| set_passes(v as u32)));

    let (toggle_key, set_toggle_key) = bind_u32(&shm, Some("toggle_key"), |h| &h.toggle_key);
    model_group.add(&hotkey_row(toggle_key, set_toggle_key));

    // --- Motion --------------------------------------------------------------------
    let motion_group = adw::PreferencesGroup::new();
    motion_group.set_title("Motion");

    let (mvec_enabled, set_mvec_enabled) = bind_bool(&shm, Some("mvec_enabled"), |h| &h.mvec_enabled);
    motion_group.add(&switch_row("Estimate motion vectors", "On by default", mvec_enabled, set_mvec_enabled));

    let (mvec_scale, set_mvec_scale) = bind_u32(&shm, Some("mvec_scale_mode"), |h| &h.mvec_scale_mode);
    motion_group.add(&combo_row("Motion units", &["Normalised", "Pixels", "UV 0..1"], mvec_scale, set_mvec_scale));
    debug_assert_eq!(mvec_scale_mode::PIXELS, 1);

    let (mvec_quality, set_mvec_quality) = bind_u32(&shm, Some("mvec_quality"), |h| &h.mvec_quality);
    motion_group.add(&combo_row("Motion quality", &["Fast", "Balanced", "Quality"], mvec_quality, set_mvec_quality));
    debug_assert_eq!(mvec_quality::BALANCED, 1);

    // --- Composition -----------------------------------------------------------------
    let comp_group = adw::PreferencesGroup::new();
    comp_group.set_title("Composition");

    let (bypass, set_bypass) = bind_bool(&shm, Some("composition_bypass"), |h| &h.composition_bypass);
    comp_group.add(&switch_row("Bypass composition", "On: the model's raw answer is the presented frame", bypass, set_bypass));

    let (transfer_strength, set_transfer_strength) = bind_float(&shm, Some("transfer_strength"), |h| &h.transfer_strength_bits);
    comp_group.add(&spin_row("Transfer strength", "How much of the model's edit reaches the frame", transfer_strength, 0.0, 1.0, 0.05, set_transfer_strength));

    let (colour_strength, set_colour_strength) = bind_float(&shm, Some("colour_strength"), |h| &h.colour_strength_bits);
    comp_group.add(&spin_row("Colour strength", "How much of the transfer is allowed to be colour, not just luminance", colour_strength, 0.0, 1.0, 0.05, set_colour_strength));

    let (max_ratio, set_max_ratio) = bind_float(&shm, Some("max_ratio"), |h| &h.max_ratio_bits);
    comp_group.add(&spin_row("Max ratio", "The most the pass may multiply/divide a pixel by", max_ratio, 1.0, 8.0, 0.1, set_max_ratio));

    let (working_scale, set_working_scale) = bind_float(&shm, Some("working_scale"), |h| &h.working_scale_bits);
    comp_group.add(&spin_row("Working scale", "Above 1.0 is supersampling", working_scale, 0.25, 2.0, 0.05, set_working_scale));

    let (downscaler_v, set_downscaler) = bind_u32(&shm, Some("scaling_downscaler"), |h| &h.scaling_downscaler);
    comp_group.add(&combo_row(
        "Supersampling filter",
        &["FSR1 (unsupported)", "Bicubic", "Catmull-Rom", "Lanczos2", "Lanczos3", "Kaiser2", "Kaiser3", "Magic"],
        downscaler_v,
        set_downscaler,
    ));
    debug_assert_eq!(downscaler::LANCZOS3, 4);

    let (reversible, set_reversible) = bind_u32(&shm, Some("reversible_mode"), |h| &h.reversible_mode);
    comp_group.add(&combo_row(
        "Reversible mode",
        &["Knee", "Neutwo", "Neutwo replace", "Hybrid", "Hybrid replace"],
        reversible,
        set_reversible,
    ));
    debug_assert_eq!(reversible_mode::KNEE, 0);

    let (hdr_mode, set_hdr_mode) = bind_u32(&shm, Some("hdr_mode"), |h| &h.hdr_mode);
    comp_group.add(&combo_row("HDR input", &["Auto", "Off", "Force float16"], hdr_mode, set_hdr_mode));

    let (colour_mode, set_colour_mode) = bind_u32(&shm, Some("colour_mode"), |h| &h.colour_mode);
    comp_group.add(&combo_row("Colour mode", &["Auto", "Force display-referred", "Force linear HDR"], colour_mode, set_colour_mode));
    debug_assert_eq!(colour_mode::AUTO, 0);

    let hdr_group = adw::PreferencesGroup::new();
    hdr_group.set_title("HDR white point");
    hdr_group.set_description(Some("Saved for HDR processing. The current capture pipeline does not yet apply these controls."));
    let (source, set_source) = bind_u32(&shm, Some("white_point_source"), |h| &h.white_point_source);
    hdr_group.add(&combo_row("White point source", &["Manual", "Measured"], source, set_source));
    let (white, set_white) = bind_float(&shm, Some("white_point"), |h| &h.white_point_bits);
    hdr_group.add(&spin_row("Manual white point", "Linear-light reference", white, 0.01, 10000.0, 0.1, set_white));
    let (scale, set_scale) = bind_float(&shm, Some("white_point_scale"), |h| &h.white_point_scale_bits);
    hdr_group.add(&spin_row("White point scale", "Multiplier", scale, 0.01, 100.0, 0.05, set_scale));
    let (trim, set_trim) = bind_float(&shm, Some("white_point_trim"), |h| &h.white_point_trim_bits);
    hdr_group.add(&spin_row("White point trim", "Calibration multiplier", trim, 0.01, 100.0, 0.05, set_trim));

    let (transfer, set_transfer) = bind_u32(&shm, Some("transfer"), |h| &h.transfer);
    comp_group.add(&combo_row("Transfer mode", &["Classic", "Matched residual", "Native + edit"], transfer, set_transfer));

    let (unlock_passes, set_unlock_passes) = bind_bool(&shm, Some("unlock_passes"), |h| &h.unlock_passes);
    comp_group.add(&switch_row("Unlock pass limit", "Allow more passes than the normal ceiling", unlock_passes, set_unlock_passes));

    let (apply_model, set_apply_model) = bind_bool(&shm, Some("apply_model"), |h| &h.apply_model);
    comp_group.add(&switch_row("Apply model edit", "Off presents the clean frame — capture/transport/round-trip still run, for an honest A/B", apply_model, set_apply_model));

    let (hold_frame, set_hold_frame) = bind_bool(&shm, Some("hold_frame"), |h| &h.hold_frame);
    comp_group.add(&switch_row("Hold frame", "Freeze the frame the pass works on, to re-run composition over the same picture", hold_frame, set_hold_frame));

    // --- Compare and debug ----------------------------------------------------------
    let debug_group = adw::PreferencesGroup::new();
    // Not "Compare & debug" -- `AdwPreferencesGroup::title` is parsed as Pango markup,
    // and a bare `&` breaks it (confirmed via a real run: "Failed to set text ...
    // Entity did not end with a semicolon").
    debug_group.set_title("Compare and debug");

    let (compare_mode, set_compare_mode) = bind_u32(&shm, Some("compare_mode"), |h| &h.compare_mode);
    debug_group.add(&combo_row("Compare mode", &["Off", "Side by side", "Wipe"], compare_mode, set_compare_mode));

    let (compare_split, set_compare_split) = bind_float(&shm, Some("compare_split"), |h| &h.compare_split_bits);
    debug_group.add(&spin_row("Compare split", "Wipe position, 0=left edge, 1=right edge", compare_split, 0.0, 1.0, 0.05, set_compare_split));

    let (compare_zoom, set_compare_zoom) = bind_float(&shm, Some("compare_zoom"), |h| &h.compare_zoom_bits);
    debug_group.add(&spin_row("Compare zoom", "", compare_zoom, 0.1, 8.0, 0.1, set_compare_zoom));

    let (compare_swap, set_compare_swap) = bind_bool(&shm, Some("compare_swap"), |h| &h.compare_swap);
    debug_group.add(&switch_row("Swap compare sides", "", compare_swap, set_compare_swap));

    let (debug_view, set_debug_view) = bind_u32(&shm, Some("debug_view"), |h| &h.debug_view);
    debug_group.add(&combo_row(
        "Debug view",
        &["Off", "Original / proxy", "Model's raw answer", "Amplified diff"],
        debug_view,
        set_debug_view,
    ));

    let toasts = adw::ToastOverlay::new();

    // Tabbed like upstream's Qt GUI, rather than one long scrolling page -- each tab
    // is still an AdwPreferencesPage, which scrolls internally on its own if its
    // content overflows the window.
    let view_stack = adw::ViewStack::new();
    view_stack.set_vexpand(true);

    let model_page = adw::PreferencesPage::new();
    model_page.add(&model_group);
    view_stack.add_titled_with_icon(&model_page, Some("model"), "Model", "applications-graphics-symbolic");

    let motion_page = adw::PreferencesPage::new();
    motion_page.add(&motion_group);
    view_stack.add_titled_with_icon(&motion_page, Some("motion"), "Motion", "camera-video-symbolic");

    let composition_page = adw::PreferencesPage::new();
    composition_page.add(&comp_group);
    composition_page.add(&hdr_group);
    view_stack.add_titled_with_icon(&composition_page, Some("composition"), "Composition", "view-paged-symbolic");

    let debug_page = adw::PreferencesPage::new();
    debug_page.add(&debug_group);
    // Short tab label -- the group's own title inside the page ("Compare and debug")
    // carries the full wording; ViewSwitcher button labels are cramped for five tabs
    // and (like AdwPreferencesGroup::title) are Pango markup, so no bare "&" either.
    view_stack.add_titled_with_icon(&debug_page, Some("debug"), "Debug", "edit-find-symbolic");

    let status_page = adw::PreferencesPage::new();
    status_page.add(&build_telemetry_group(&shm));
    status_page.add(&build_status_group(&shm, &toasts));
    view_stack.add_titled_with_icon(&status_page, Some("status"), "Status", "network-transmit-receive-symbolic");

    let setup_page = build_setup_page(&toasts);
    view_stack.add_titled_with_icon(&setup_page, Some("setup"), "Setup", "preferences-system-symbolic");

    // First-run flow: `nvngx_dlssnr.dll` missing means neural rendering can't work at
    // all yet (fail-open just presents untouched frames, no error a first-time user
    // would ever see) -- land on Setup instead of Model, with a banner explaining why,
    // rather than a silently-inert app.
    let missing_ngx = !crate::binaries::dir().join("nvngx_dlssnr.dll").is_file();
    if missing_ngx {
        view_stack.set_visible_child_name("setup");
    }
    let banner = adw::Banner::new("NVIDIA NGX binaries are missing -- neural rendering can't run without them");
    banner.set_button_label(Some("Open Setup"));
    banner.set_revealed(missing_ngx);
    {
        let view_stack = view_stack.clone();
        let banner_for_closure = banner.clone();
        banner.connect_button_clicked(move |_| {
            view_stack.set_visible_child_name("setup");
            banner_for_closure.set_revealed(false);
        });
    }

    // Deprecated since libadwaita 1.4 in favor of AdwBreakpoint, but that replacement
    // needs a newer libadwaita than this project targets (see the gtk4/libadwaita
    // feature-flag gotcha elsewhere in this codebase) -- ViewSwitcherTitle/Bar still
    // work and are the version-compatible choice.
    let switcher_title = adw::ViewSwitcherTitle::new();
    switcher_title.set_stack(Some(&view_stack));
    switcher_title.set_title("NeuralForge");

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher_title));
    let about_btn = gtk4::Button::builder().icon_name("help-about-symbolic").tooltip_text("About").build();
    header.pack_end(&about_btn);

    // Collapses into the header's switcher above when there's room, otherwise
    // reveals this bar -- the standard adaptive pattern so the window can still be
    // narrowed without the tab bar becoming unusable.
    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_stack(Some(&view_stack));
    switcher_title.bind_property("title-visible", &switcher_bar, "reveal").build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&banner);
    content.append(&view_stack);
    content.append(&switcher_bar);
    toasts.set_child(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("NeuralForge")
        .default_width(620)
        .default_height(700)
        .content(&toasts)
        .build();

    {
        let window = window.clone();
        about_btn.connect_clicked(move |_| {
            let dialog = adw::AboutDialog::builder()
                .application_name("NeuralForge")
                .version(env!("CARGO_PKG_VERSION"))
                .developers(vec!["Linnard Alex Brown Jr."])
                .comments("Vulkan layer and settings GUI for running NVIDIA DLSS 5 Neural Rendering on Linux/Proton games.")
                .build();
            dialog.add_acknowledgement_section(
                Some("Built with"),
                &["Claude Code (Anthropic)", "Codex (OpenAI)"],
            );
            dialog.present(Some(&window));
        });
    }

    window.present();
}

/// A read-only status group, refreshed on a timer -- helper/layer liveness, frame
/// counts. Nothing here is a setting; it only ever reads.
///
/// The one exception is the "NGX binaries" row's Import button: unlike everything
/// else here, it's an action, not a live readout, because it's the only place besides
/// `neuralforge-cli import-binaries` to get NVIDIA's DLLs into `binaries_dir()` -- there's
/// no separate menu for it.
fn profile_names() -> Vec<String> {
    let mut names: Vec<String> = neuralforge_supervisor::profiles::load_all().into_keys().collect();
    names.sort();
    names
}

/// Rebuilds the combo's model from disk -- called on init and after every
/// save/delete, since the set of saved profiles can only change through this same
/// window (single-user, single-process local GUI).
fn refresh_profile_combo(combo: &adw::ComboRow) {
    let names = profile_names();
    if names.is_empty() {
        combo.set_model(Some(&gtk4::StringList::new(&["No saved profiles"])));
        combo.set_sensitive(false);
    } else {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        combo.set_model(Some(&gtk4::StringList::new(&refs)));
        combo.set_sensitive(true);
    }
}

fn build_ngx_group(toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("NVIDIA NGX binaries");
    group.set_description(Some("From your own NVIDIA driver/SDK install -- this project doesn't and can't ship them"));

    let mut status_rows: Vec<(String, adw::ActionRow, gtk4::Image)> = Vec::new();
    for (name, present) in crate::binaries::status() {
        let row = adw::ActionRow::new();
        row.set_title(name);
        row.set_subtitle(if present { "present" } else { "missing" });
        let icon = gtk4::Image::from_icon_name(if present { "emblem-ok-symbolic" } else { "dialog-warning-symbolic" });
        row.add_prefix(&icon);
        group.add(&row);
        status_rows.push((name.to_string(), row, icon));
    }

    let import_row = adw::ActionRow::new();
    import_row.set_title("Import");
    import_row.set_subtitle("Copy the DLLs above from a folder (an extracted NVIDIA driver/SDK) in one step");
    let import_button = gtk4::Button::with_label("Import…");
    import_button.set_valign(gtk4::Align::Center);
    import_row.add_suffix(&import_button);
    import_row.set_activatable_widget(Some(&import_button));
    group.add(&import_row);

    {
        let toasts = toasts.clone();
        import_button.connect_clicked(move |button| {
            let toasts = toasts.clone();
            let status_rows = status_rows.clone();
            let parent = button.root().and_downcast::<gtk4::Window>();
            let dialog = gtk4::FileDialog::builder().title("Select folder containing NVIDIA NGX DLLs").build();
            dialog.select_folder(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                let Ok(folder) = result else { return };
                let Some(path) = folder.path() else { return };
                match crate::binaries::import_from(&path) {
                    Ok(0) => toasts.add_toast(adw::Toast::new("No matching DLLs found in that folder")),
                    Ok(n) => {
                        toasts.add_toast(adw::Toast::new(&format!("Imported {n} file(s) -- restart the helper to load them")));
                        for (name, row, icon) in &status_rows {
                            let present = crate::binaries::dir().join(name).is_file();
                            row.set_subtitle(if present { "present" } else { "missing" });
                            icon.set_icon_name(Some(if present { "emblem-ok-symbolic" } else { "dialog-warning-symbolic" }));
                        }
                    }
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Import failed: {e}"))),
                }
            });
        });
    }

    group
}

fn build_runner_group(toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Compatibility tool");
    group.set_description(Some("Needs DXVK-NVAPI (Proton-CachyOS, Proton-GE) or a system Wine with it installed -- \
                            Valve's stock Proton builds don't bundle it"));

    let mut options: Vec<(String, String)> = neuralforge_supervisor::runners::discover_proton()
        .into_iter()
        .map(|runner| (runner.name, runner.path.to_string_lossy().into_owned()))
        .collect();
    if let Some(wine) = neuralforge_supervisor::runners::find_wine() {
        options.push(("System Wine".to_string(), wine.to_string_lossy().into_owned()));
    }

    let combo = adw::ComboRow::new();
    combo.set_title("Runner");
    if options.is_empty() {
        combo.set_model(Some(&gtk4::StringList::new(&["No compatibility tool found"])));
        combo.set_sensitive(false);
    } else {
        let cfg = neuralforge_supervisor::Config::load();
        let names: Vec<&str> = options.iter().map(|(name, _)| name.as_str()).collect();
        combo.set_model(Some(&gtk4::StringList::new(&names)));
        let selected = options.iter().position(|(_, path)| *path == cfg.runner_path).unwrap_or(0);
        combo.set_selected(selected as u32);
    }
    group.add(&combo);

    if !options.is_empty() {
        let toasts = toasts.clone();
        combo.connect_selected_notify(move |combo| {
            let Some((name, path)) = options.get(combo.selected() as usize) else { return };
            let mut cfg = neuralforge_supervisor::Config::load();
            cfg.runner_type = if name == "System Wine" { "wine".to_string() } else { "proton".to_string() };
            cfg.runner_path = path.clone();
            match cfg.save() {
                Ok(()) => toasts.add_toast(adw::Toast::new(&format!("Runner set to {name} -- restart the helper to use it"))),
                Err(e) => toasts.add_toast(adw::Toast::new(&format!("Failed to save: {e}"))),
            }
        });
    }

    group
}

fn build_install_group(toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Steam games");
    group.set_description(Some("Copies this AppImage's own layer and binaries into persistent user storage, so Vulkan \
                            can still find them once the AppImage itself isn't running"));

    let row = adw::ActionRow::new();
    row.set_title("Install layer for Steam games");
    let install_button = gtk4::Button::with_label("Install…");
    install_button.set_valign(gtk4::Align::Center);
    install_button.add_css_class("suggested-action");

    // `APPDIR` is the AppImage runtime's own env var for the live mounted AppDir --
    // only set when this GUI is actually running from inside an AppImage, which is
    // also the only case this button makes sense in (a `cargo run` dev build has no
    // AppDir to install from).
    let appdir = std::env::var("APPDIR").ok();
    row.set_subtitle(match &appdir {
        Some(dir) => dir.as_str(),
        None => "Only available when running from the AppImage",
    });
    if appdir.is_none() {
        install_button.set_sensitive(false);
    }
    row.add_suffix(&install_button);
    group.add(&row);

    if let Some(dir) = appdir {
        let toasts = toasts.clone();
        install_button.connect_clicked(move |_| match neuralforge_supervisor::install::install(std::path::Path::new(&dir)) {
            Ok(report) => toasts.add_toast(adw::Toast::new(&format!("Installed to {}", report.root.display()))),
            Err(e) => toasts.add_toast(adw::Toast::new(&format!("Install failed: {e}"))),
        });
    }

    group
}

/// The exact Steam launch-option string for these settings -- pulled out of the
/// closure below so it's a plain, unit-testable function instead of only ever being
/// exercised live through GTK signal handlers.
fn launch_option(target_exe: &str, dmabuf: bool) -> String {
    let dmabuf = u32::from(dmabuf);
    let target_exe = target_exe.trim();
    if target_exe.is_empty() {
        format!("NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF={dmabuf} %command%")
    } else {
        format!("NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF={dmabuf} NEURALFORGE_TARGET_EXE={target_exe} %command%")
    }
}

fn build_launch_option_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Steam launch option");
    group.set_description(Some("Paste this into the game's Properties -> Launch Options in Steam"));

    let exe_row = adw::EntryRow::new();
    exe_row.set_title("Target executable (optional, for a multi-process game)");
    group.add(&exe_row);

    let dmabuf_row = adw::SwitchRow::new();
    dmabuf_row.set_title("DMA-BUF transport");
    dmabuf_row.set_subtitle("Off is the known-good baseline");
    group.add(&dmabuf_row);

    let preview_row = adw::ActionRow::new();
    preview_row.set_title("Launch option");
    preview_row.add_css_class("property");
    let copy_button = gtk4::Button::with_label("Copy");
    copy_button.set_valign(gtk4::Align::Center);
    preview_row.add_suffix(&copy_button);
    group.add(&preview_row);

    let exe_row_for_build = exe_row.clone();
    let dmabuf_row_for_build = dmabuf_row.clone();
    let build_option = std::rc::Rc::new(move || launch_option(&exe_row_for_build.text(), dmabuf_row_for_build.is_active()));

    preview_row.set_subtitle(&build_option());

    {
        let preview_row = preview_row.clone();
        let build_option = std::rc::Rc::clone(&build_option);
        exe_row.connect_changed(move |_| preview_row.set_subtitle(&build_option()));
    }
    {
        let preview_row = preview_row.clone();
        let build_option = std::rc::Rc::clone(&build_option);
        dmabuf_row.connect_active_notify(move |_| preview_row.set_subtitle(&build_option()));
    }
    copy_button.connect_clicked(move |button| {
        button.display().clipboard().set_text(&build_option());
    });

    group
}

fn build_setup_page(toasts: &adw::ToastOverlay) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(&build_ngx_group(toasts));
    page.add(&build_runner_group(toasts));
    page.add(&build_install_group(toasts));
    page.add(&build_launch_option_group());
    page
}

/// One sample of the three timing series the sparkline plots, all already published
/// by the layer/helper for `neuralforge-cli shmctl status` -- this just samples them
/// on a faster timer than the once-a-second status labels above need, and keeps the
/// last few seconds of them for the drawing area to plot.
#[derive(Clone, Copy)]
struct TelemetrySample {
    layer_ms: f32,
    helper_round_trip_ms: f32,
    helper_eval_ms: f32,
}

const TELEMETRY_INTERVAL_MS: u32 = 200;
const TELEMETRY_WINDOW_SECS: u32 = 5;
const TELEMETRY_SAMPLES: usize = (TELEMETRY_WINDOW_SECS * 1000 / TELEMETRY_INTERVAL_MS) as usize;

fn draw_telemetry_sparkline(cr: &gtk4::cairo::Context, width: i32, height: i32, history: &std::collections::VecDeque<TelemetrySample>) {
    let (width, height) = (f64::from(width), f64::from(height));
    let _ = cr.save();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();

    if history.len() < 2 {
        let _ = cr.restore();
        return;
    }

    // A fixed ceiling (not the window's own max) so the line's height means the same
    // thing frame to frame instead of visually flattening every series out whenever
    // one of them briefly spikes -- 20ms is comfortably above a healthy per-stage
    // budget at the frame rates this project targets, without being so tall that
    // ordinary sub-millisecond noise disappears into the bottom pixel row.
    const CEILING_MS: f32 = 20.0;
    let plot = |cr: &gtk4::cairo::Context, pick: fn(&TelemetrySample) -> f32| {
        for (i, sample) in history.iter().enumerate() {
            let x = width * (i as f64) / ((history.len() - 1) as f64);
            let y = height * (1.0 - f64::from(pick(sample).min(CEILING_MS) / CEILING_MS));
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        let _ = cr.stroke();
    };

    cr.set_line_width(1.6);
    cr.set_source_rgb(0.988, 0.686, 0.243); // helper round trip -- amber
    plot(cr, |s| s.helper_round_trip_ms);
    cr.set_source_rgb(0.204, 0.780, 0.678); // model eval -- teal
    plot(cr, |s| s.helper_eval_ms);
    cr.set_source_rgb(0.596, 0.478, 0.953); // layer (capture+compose) -- violet, matches the app's own accent
    plot(cr, |s| s.layer_ms);

    let _ = cr.restore();
}

fn legend_label(text: &str, rgb: (f64, f64, f64)) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let swatch = gtk4::DrawingArea::new();
    swatch.set_content_width(10);
    swatch.set_content_height(10);
    swatch.set_valign(gtk4::Align::Center);
    swatch.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgb(rgb.0, rgb.1, rgb.2);
        cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
        let _ = cr.fill();
    });
    row.append(&swatch);
    row.append(&gtk4::Label::new(Some(text)));
    row
}

fn build_telemetry_group(shm: &std::sync::Arc<neuralforge_protocol::mapping::Mapping>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Telemetry");
    group.set_description(Some("Live from the running helper/layer -- all zero until a targeted game attaches"));

    let model_row = adw::ActionRow::new();
    model_row.set_title("Model");
    let game_row = adw::ActionRow::new();
    game_row.set_title("Game");
    let fps_row = adw::ActionRow::new();
    fps_row.set_title("Frame rate");
    group.add(&model_row);
    group.add(&game_row);
    group.add(&fps_row);

    let legend = gtk4::Box::new(gtk4::Orientation::Horizontal, 16);
    legend.set_margin_top(10);
    legend.set_margin_start(10);
    legend.set_margin_end(10);
    legend.append(&legend_label("Layer", (0.596, 0.478, 0.953)));
    legend.append(&legend_label("Helper round trip", (0.988, 0.686, 0.243)));
    legend.append(&legend_label("Model eval", (0.204, 0.780, 0.678)));

    let sparkline = gtk4::DrawingArea::new();
    // A minimum, not a ceiling: `AdwPreferencesGroup` still stretches this row taller
    // than 80px on a short page with room to spare (confirmed visually; `vexpand`
    // false the whole way down this box's ancestor chain didn't stop it either).
    // Harmless -- `draw_telemetry_sparkline` normalizes against whatever height it's
    // actually given each draw, so the plot still reads correctly at any size.
    sparkline.set_size_request(-1, 80);
    sparkline.set_hexpand(true);
    sparkline.set_vexpand(false);
    sparkline.set_margin_start(10);
    sparkline.set_margin_end(10);
    sparkline.set_margin_bottom(10);

    // One card-styled box for both, matching the boxed-row look every other group in
    // this app already has -- adding `legend`/`sparkline` straight to `group` instead
    // makes each its own bare, unstyled top-level row with layout that doesn't match
    // (confirmed visually: `AdwPreferencesGroup` gives each direct child list-row
    // spacing meant for `AdwActionRow`-shaped content, not a fixed-height drawing
    // area, so `set_content_height` alone doesn't produce the intended fixed size).
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    card.add_css_class("card");
    card.set_vexpand(false);
    card.set_valign(gtk4::Align::Start);
    card.set_margin_top(6);
    card.set_margin_bottom(6);
    card.set_margin_start(12);
    card.set_margin_end(12);
    card.append(&legend);
    card.append(&sparkline);
    group.add(&card);

    let history = std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::<TelemetrySample>::with_capacity(TELEMETRY_SAMPLES)));
    {
        let history = std::rc::Rc::clone(&history);
        sparkline.set_draw_func(move |_, cr, width, height| draw_telemetry_sparkline(cr, width, height, &history.borrow()));
    }

    let shm_for_timer = std::sync::Arc::clone(shm);
    let sparkline_for_timer = sparkline.clone();
    let mut last_helper_frames: Option<u64> = None;
    let mut last_layer_frames: Option<u64> = None;
    glib::timeout_add_local(std::time::Duration::from_millis(u64::from(TELEMETRY_INTERVAL_MS)), move || {
        let hdr = shm_for_timer.header();

        model_row.set_subtitle(if hdr.model_up.load(Ordering::Relaxed) != 0 { "loaded" } else { "not loaded" });
        let game = hdr.game_name();
        game_row.set_subtitle(if game.is_empty() { "none attached" } else { &game });

        let helper_frames = (u64::from(hdr.helper_frames_hi.load(Ordering::Relaxed)) << 32) | u64::from(hdr.helper_frames_lo.load(Ordering::Relaxed));
        let layer_frames = (u64::from(hdr.layer_frames_hi.load(Ordering::Relaxed)) << 32) | u64::from(hdr.layer_frames_lo.load(Ordering::Relaxed));
        let per_second = 1000.0 / f64::from(TELEMETRY_INTERVAL_MS);
        let helper_fps = last_helper_frames.map(|prev| (helper_frames.saturating_sub(prev)) as f64 * per_second);
        let layer_fps = last_layer_frames.map(|prev| (layer_frames.saturating_sub(prev)) as f64 * per_second);
        last_helper_frames = Some(helper_frames);
        last_layer_frames = Some(layer_frames);
        match (layer_fps, helper_fps) {
            (Some(l), Some(h)) => fps_row.set_subtitle(&format!("layer {l:.1}/s, helper {h:.1}/s")),
            _ => fps_row.set_subtitle("—"),
        }

        let sample = TelemetrySample {
            layer_ms: f32::from_bits(hdr.layer_ms_bits.load(Ordering::Relaxed)),
            helper_round_trip_ms: f32::from_bits(hdr.helper_upload_ms_bits.load(Ordering::Relaxed))
                + f32::from_bits(hdr.helper_eval_ms_bits.load(Ordering::Relaxed))
                + f32::from_bits(hdr.helper_readback_ms_bits.load(Ordering::Relaxed)),
            helper_eval_ms: f32::from_bits(hdr.helper_eval_ms_bits.load(Ordering::Relaxed)),
        };
        {
            let mut history = history.borrow_mut();
            if history.len() == TELEMETRY_SAMPLES {
                history.pop_front();
            }
            history.push_back(sample);
        }
        sparkline_for_timer.queue_draw();

        glib::ControlFlow::Continue
    });

    group
}

fn build_status_group(shm: &std::sync::Arc<neuralforge_protocol::mapping::Mapping>, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Status");

    let helper_row = adw::ActionRow::new();
    helper_row.set_title("Helper");
    let start_stop_button = gtk4::Button::with_label("Start");
    start_stop_button.set_valign(gtk4::Align::Center);
    helper_row.add_suffix(&start_stop_button);
    let layer_row = adw::ActionRow::new();
    layer_row.set_title("Layer");
    group.add(&helper_row);
    group.add(&layer_row);

    let shm_for_timer = std::sync::Arc::clone(shm);
    let start_stop_button_for_timer = start_stop_button.clone();
    glib::timeout_add_seconds_local(1, move || {
        let hdr = shm_for_timer.header();
        let helper_state = hdr.helper_state.load(Ordering::Relaxed);
        helper_row.set_subtitle(helper_state_label(helper_state));
        let attached = hdr.layer_attached.load(Ordering::Relaxed) != 0;
        layer_row.set_subtitle(if attached { "attached" } else { "not attached" });
        // Keyed off the actual OS-level pid-file check (what start/stop manage), not
        // the SHM helper_state above -- those can briefly disagree right after a
        // start/stop (e.g. STARTING vs. the process not existing yet) and the button
        // should reflect what clicking it will actually do, not the helper's own
        // self-reported state.
        if start_stop_button_for_timer.is_sensitive() {
            start_stop_button_for_timer.set_label(if neuralforge_supervisor::is_running().is_some() { "Stop" } else { "Start" });
        }
        glib::ControlFlow::Continue
    });

    {
        let toasts = toasts.clone();
        start_stop_button.connect_clicked(move |button| {
            let toasts = toasts.clone();
            if neuralforge_supervisor::is_running().is_some() {
                // Stopping waits up to 5s for a graceful exit before escalating to
                // SIGKILL (see neuralforge_supervisor::stop) -- a brief, bounded main-thread
                // block on an explicit user click, not worth the async plumbing this
                // small a GUI doesn't otherwise need.
                button.set_sensitive(false);
                button.set_label("Stopping…");
                match neuralforge_supervisor::stop(std::time::Duration::from_secs(5)) {
                    Ok(()) => toasts.add_toast(adw::Toast::new("Helper stopped")),
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Stop failed: {e}"))),
                }
                button.set_sensitive(true);
            } else {
                let cfg = neuralforge_supervisor::Config::load();
                match neuralforge_supervisor::start(&cfg) {
                    Ok(started) => toasts.add_toast(adw::Toast::new(&format!("Helper started (pid {})", started.pid))),
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Start failed: {e}"))),
                }
            }
        });
    }

    let binaries_row = adw::ActionRow::new();
    binaries_row.set_title("NGX binaries");
    binaries_row.set_subtitle(&binaries_status_subtitle());
    let import_button = gtk4::Button::with_label("Import…");
    import_button.set_valign(gtk4::Align::Center);
    binaries_row.add_suffix(&import_button);
    group.add(&binaries_row);

    {
        let toasts = toasts.clone();
        import_button.connect_clicked(move |button| {
            let toasts = toasts.clone();
            let binaries_row = binaries_row.clone();
            let parent = button.root().and_downcast::<gtk4::Window>();
            let dialog = gtk4::FileDialog::builder().title("Select folder containing NVIDIA NGX DLLs").build();
            dialog.select_folder(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                let Ok(folder) = result else { return };
                let Some(path) = folder.path() else { return };
                match crate::binaries::import_from(&path) {
                    Ok(0) => toasts.add_toast(adw::Toast::new("No matching DLLs found in that folder")),
                    Ok(n) => {
                        toasts.add_toast(adw::Toast::new(&format!("Imported {n} file(s) -- restart the helper to load them")));
                        binaries_row.set_subtitle(&binaries_status_subtitle());
                    }
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Import failed: {e}"))),
                }
            });
        });
    }

    let settings_row = adw::ActionRow::new();
    settings_row.set_title("Settings");
    settings_row.set_subtitle("Reset every tuning value to its default");
    let reset_button = gtk4::Button::with_label("Reset…");
    reset_button.add_css_class("destructive-action");
    reset_button.set_valign(gtk4::Align::Center);
    settings_row.add_suffix(&reset_button);
    group.add(&settings_row);

    {
        let shm = std::sync::Arc::clone(shm);
        let toasts = toasts.clone();
        reset_button.connect_clicked(move |button| {
            let shm = std::sync::Arc::clone(&shm);
            let toasts = toasts.clone();
            let parent = button.root().and_downcast::<gtk4::Window>();
            let dialog = adw::AlertDialog::builder()
                .heading("Reset all settings?")
                .body("Every tuning value returns to its default. The running helper/layer \
                       session (frame counters, transport state) is not affected. Restart \
                       NeuralForge afterward to see the reset values in this window.")
                .default_response("cancel")
                .close_response("cancel")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("reset", "Reset");
            dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
            dialog.choose(parent.as_ref(), None::<&gio::Cancellable>, move |response| {
                if response != "reset" { return; }
                // Reset the live mapping the running helper/layer are already
                // attached to, then overwrite config.ini with the same defaults so a
                // restart doesn't just reload the values this just cleared -- the
                // same two places `persist_one` already keeps in sync for a single
                // setting, done here for all of them at once.
                shm.header().reset_persisted_settings();
                let mut cfg = neuralforge_supervisor::Config::load();
                for (name, value) in neuralforge_protocol::persist::snapshot(shm.header()) {
                    cfg.settings.insert(name, value);
                }
                match cfg.save() {
                    Ok(()) => toasts.add_toast(adw::Toast::new("Settings reset -- restart NeuralForge to see it reflected here")),
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Reset the live session, but saving config.ini failed: {e}"))),
                }
            });
        });
    }

    let save_profile_row = adw::EntryRow::new();
    save_profile_row.set_title("Save current as");
    let save_profile_button = gtk4::Button::with_label("Save");
    save_profile_button.set_valign(gtk4::Align::Center);
    save_profile_button.add_css_class("suggested-action");
    save_profile_row.add_suffix(&save_profile_button);
    group.add(&save_profile_row);

    let profile_combo = adw::ComboRow::new();
    profile_combo.set_title("Load profile");
    refresh_profile_combo(&profile_combo);
    let load_profile_button = gtk4::Button::with_label("Load");
    load_profile_button.set_valign(gtk4::Align::Center);
    profile_combo.add_suffix(&load_profile_button);
    let delete_profile_button = gtk4::Button::with_label("Delete");
    delete_profile_button.add_css_class("destructive-action");
    delete_profile_button.set_valign(gtk4::Align::Center);
    profile_combo.add_suffix(&delete_profile_button);
    group.add(&profile_combo);

    {
        let shm = std::sync::Arc::clone(shm);
        let toasts = toasts.clone();
        let profile_combo = profile_combo.clone();
        let save_profile_row = save_profile_row.clone();
        save_profile_button.connect_clicked(move |_| {
            let name = save_profile_row.text().trim().to_string();
            if name.is_empty() {
                toasts.add_toast(adw::Toast::new("Enter a name before saving"));
                return;
            }
            let settings = neuralforge_protocol::persist::snapshot(shm.header());
            match neuralforge_supervisor::profiles::save_profile(&name, settings) {
                Ok(()) => {
                    toasts.add_toast(adw::Toast::new(&format!("Saved profile \"{name}\"")));
                    save_profile_row.set_text("");
                    refresh_profile_combo(&profile_combo);
                }
                Err(e) => toasts.add_toast(adw::Toast::new(&format!("Save failed: {e}"))),
            }
        });
    }

    {
        let shm = std::sync::Arc::clone(shm);
        let toasts = toasts.clone();
        let profile_combo = profile_combo.clone();
        load_profile_button.connect_clicked(move |_| {
            let names = profile_names();
            let Some(name) = names.get(profile_combo.selected() as usize) else {
                toasts.add_toast(adw::Toast::new("No profile selected"));
                return;
            };
            let profiles = neuralforge_supervisor::profiles::load_all();
            let Some(settings) = profiles.get(name) else { return };
            neuralforge_protocol::persist::apply(shm.header(), settings);
            // Same reasoning as the reset button above: applying to the live header
            // only affects the running session, so also fold the result into
            // config.ini via a fresh snapshot so it survives a reboot too.
            let mut cfg = neuralforge_supervisor::Config::load();
            cfg.settings = neuralforge_protocol::persist::snapshot(shm.header());
            let message = match cfg.save() {
                Ok(()) => format!("Loaded profile \"{name}\" -- restart NeuralForge to see it reflected here"),
                Err(e) => format!("Applied to the running session, but saving config.ini failed: {e}"),
            };
            toasts.add_toast(adw::Toast::new(&message));
        });
    }

    {
        let toasts = toasts.clone();
        let profile_combo = profile_combo.clone();
        delete_profile_button.connect_clicked(move |_| {
            let names = profile_names();
            let Some(name) = names.get(profile_combo.selected() as usize).cloned() else {
                toasts.add_toast(adw::Toast::new("No profile selected"));
                return;
            };
            match neuralforge_supervisor::profiles::delete_profile(&name) {
                Ok(true) => {
                    toasts.add_toast(adw::Toast::new(&format!("Deleted profile \"{name}\"")));
                    refresh_profile_combo(&profile_combo);
                }
                Ok(false) => toasts.add_toast(adw::Toast::new("Profile already gone")),
                Err(e) => toasts.add_toast(adw::Toast::new(&format!("Delete failed: {e}"))),
            }
        });
    }

    group
}

fn binaries_status_subtitle() -> String {
    if crate::binaries::dir().join("nvngx_dlssnr.dll").is_file() {
        "nvngx_dlssnr.dll present".to_string()
    } else {
        "nvngx_dlssnr.dll missing".to_string()
    }
}

fn helper_state_label(state: u32) -> &'static str {
    use neuralforge_protocol::enums::helper_state::*;
    match state {
        STARTING => "starting",
        NO_VULKAN => "no NVIDIA Vulkan device",
        NO_BINARIES => "NGX binaries missing",
        MODEL_FAILED => "model failed to load",
        RUNNING => "running",
        STOPPED => "stopped",
        _ => "unknown",
    }
}

fn build_error_window(app: &adw::Application) {
    let status = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Couldn't open the shared-memory mapping")
        .description("Check the helper's log; neuralforge-cli doctor may also help.")
        .build();
    let window = adw::ApplicationWindow::builder().application(app).title("NeuralForge").content(&status).build();
    window.present();
}

#[cfg(test)] mod hotkey_tests {
    use super::*;
    #[test] fn hardware_codes_are_converted_without_underflow() {
        assert_eq!(evdev_keycode(95),Some(87)); // F11: XKB -> Linux evdev.
        assert_eq!(evdev_keycode(38),Some(30)); // physical A key on evdev.
        assert_eq!(evdev_keycode(0),None);
        assert_eq!(evdev_keycode(8),None);
        assert_eq!(evdev_keycode(u32::MAX),None);
    }
}

#[cfg(test)]
mod launch_option_tests {
    use super::*;

    #[test]
    fn matches_the_documented_baseline_with_no_target_exe() {
        assert_eq!(launch_option("", false), "NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=0 %command%");
    }

    #[test]
    fn includes_target_exe_when_given() {
        assert_eq!(launch_option("GTA5_Enhanced.exe", false), "NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=0 NEURALFORGE_TARGET_EXE=GTA5_Enhanced.exe %command%");
    }

    #[test]
    fn dmabuf_toggle_changes_only_that_field() {
        assert_eq!(launch_option("", true), "NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=1 %command%");
    }

    #[test]
    fn trims_whitespace_around_target_exe() {
        assert_eq!(launch_option("  GTA5_Enhanced.exe  ", false), "NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=0 NEURALFORGE_TARGET_EXE=GTA5_Enhanced.exe %command%");
    }

    #[test]
    fn whitespace_only_target_exe_is_treated_as_empty() {
        assert_eq!(launch_option("   ", false), "NEURALFORGE_ENABLE=1 NEURALFORGE_DMABUF=0 %command%");
    }
}
