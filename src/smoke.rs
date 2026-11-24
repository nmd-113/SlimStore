//! Explicit, opt-in read-only GUI checks used by scripts/verify.ps1.
use crate::app::INSTALL_QUEUE_PAGE;
use crate::{ActivityItem, Artwork, MainWindow, errors::describe_exit};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{cell::Cell, io::Write, rc::Rc, time::Duration};

fn report(message: &str) {
    if let Some(path) = std::env::var_os("SLIMSTORE_SMOKE_REPORT")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

pub fn install(ui: &MainWindow) -> slint::Timer {
    let timer = slint::Timer::default();
    let Some(mode) = std::env::var_os("SLIMSTORE_GUI_SMOKE") else {
        return timer;
    };
    let mode = mode.to_string_lossy().into_owned();
    let weak = ui.as_weak();
    let phase = Rc::new(Cell::new(0));
    let ticks = Rc::new(Cell::new(0));
    report(&format!("START {mode}"));
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ticks.set(ticks.get() + 1);
            let result = (|| -> Result<bool, String> {
                if ticks.get() > 180 {
                    return Err(format!(
                        "Timeout in phase {}: {}",
                        phase.get(),
                        ui.get_status()
                    ));
                }
                if mode.starts_with("preview-") {
                    if phase.get() == 0 {
                        match mode.as_str() {
                            "preview-home" => {}
                            "preview-discover" => ui.invoke_navigate(1),
                            "preview-categories" => ui.invoke_navigate(2),
                            "preview-installed" => ui.invoke_navigate(3),
                            "preview-updates" => ui.invoke_navigate(4),
                            "preview-activity" => {
                                if ui.get_busy() {
                                    return Ok(false);
                                }
                                ui.invoke_navigate(5);
                                ui.set_activity(ModelRc::new(VecModel::from(
                                    [
                                        ("Dolphin", "Failed", 0x80190194),
                                        ("CPUID CPU-Z", "Completed", 0),
                                        ("Bandizip", "Failed", 0x8a150011),
                                    ]
                                    .map(|(name, status, code)| ActivityItem {
                                        title: format!("{name} · install").into(),
                                        status: status.into(),
                                        summary: describe_exit(code).into(),
                                        detail: format!("{} (0x{code:08X})", describe_exit(code)).into(),
                                        ..Default::default()
                                    })
                                    .to_vec(),
                                )));
                            }
                            "preview-settings" => ui.invoke_navigate(6),
                            "preview-details" => ui.invoke_open_app("Mozilla.Firefox".into()),
                            "preview-gallery" => ui.invoke_open_app("Microsoft.PowerToys".into()),
                            "preview-handbrake" => {
                                ui.invoke_navigate(1);
                                ui.invoke_open_app("HandBrake.HandBrake".into());
                            }
                            "preview-light" => {
                                ui.set_theme_index(1);
                                ui.invoke_settings_changed();
                            }
                            "preview-grid" => ui.invoke_navigate(1),
                            "preview-list" => {
                                ui.invoke_navigate(1);
                                ui.set_list_mode(true);
                            }
                            _ => return Err(format!("Unknown preview mode: {mode}")),
                        }
                        phase.set(-1);
                        report("PASS PREVIEW READY");
                    }
                    return Ok(false);
                }
                if matches!(mode.as_str(), "install-review" | "uninstall-review") {
                    if ui.get_busy() {
                        return Ok(false);
                    }
                    let uninstall = mode == "uninstall-review";
                    let (ids, action, title) = if uninstall {
                        (["7zip.7zip", "Microsoft.VisualStudioCode"], "uninstall-selected", "Uninstall 2")
                    } else {
                        (["Mozilla.Firefox", "Microsoft.PowerToys"], "install-selected", "Install 2")
                    };
                    let selection_count = || {
                        if uninstall { ui.get_uninstall_count() } else { ui.get_install_count() }
                    };
                    match phase.get() {
                        0 => {
                            if uninstall {
                                ui.invoke_navigate(3);
                            }
                        }
                        1 => {
                            for id in ids {
                                ui.invoke_select_app(id.into(), true);
                            }
                            if selection_count() != 2 {
                                return Err("Two packages were not retained in the selection".into());
                            }
                            if !uninstall {
                                ui.invoke_navigate(1);
                            }
                            ui.set_list_mode(true);
                            if uninstall {
                                ui.invoke_action(action.into());
                                phase.set(2);
                            } else {
                                ui.invoke_navigate(INSTALL_QUEUE_PAGE);
                            }
                        }
                        2 => {
                            if ui.get_result_count() != 2 || ui.get_rows().row_count() != 2 {
                                return Err("Install Queue did not show both selected packages".into());
                            }
                            ui.invoke_action(action.into());
                        }
                        _ => {
                            if !ui.get_confirm_visible()
                                || !ui.get_confirm_title().contains(title)
                                || ids.iter().any(|id| !ui.get_confirm_text().contains(id))
                                || ui.get_confirm_agreement_required() == uninstall
                                || ui.get_package_consent()
                            {
                                return Err(format!("Batch review did not list both verified apps: {}", ui.get_status()));
                            }
                            ui.invoke_confirmed(false);
                            if ui.get_confirm_visible() || ui.get_operation_count() != 0 || selection_count() != 2 {
                                return Err("Cancelling the review altered selection or the queue".into());
                            }
                            report("PASS multi-selection across views, exact live preflight, and cancellation");
                            return Ok(true);
                        }
                    }
                    phase.set(phase.get() + 1);
                    return Ok(false);
                }
                if mode == "install-queue" {
                    if ui.get_busy() {
                        return Ok(false);
                    }
                    let removed = "Mozilla.Firefox";
                    let kept = "Microsoft.PowerToys";
                    match phase.get() {
                        0 => {
                            for id in [removed, kept] {
                                ui.invoke_select_app(id.into(), true);
                            }
                            ui.invoke_action("install-selected".into());
                            if ui.get_busy() || ui.get_confirm_visible() || ui.get_operation_count() != 0 {
                                return Err("The catalog bypassed the Install Queue".into());
                            }
                            ui.invoke_navigate(INSTALL_QUEUE_PAGE);
                            if ui.get_install_count() != 2 || ui.get_rows().row_count() != 2
                                || ui.get_columns() != 1 || ui.get_list_mode()
                            {
                                return Err("Install Queue failed to show both apps in one list".into());
                            }
                            ui.invoke_open_app(kept.into());
                        }
                        1 => {
                            if ui.get_page() != 7 || ui.get_detail().id != kept {
                                return Err("Install Queue could not open app details".into());
                            }
                            ui.invoke_back();
                            ui.invoke_select_app(removed.into(), false);
                            if ui.get_page() != INSTALL_QUEUE_PAGE || ui.get_install_count() != 1
                                || ui.get_rows().row_count() != 1
                                || ui.get_rows().row_data(0).is_none_or(|row| row.first.id != kept)
                            {
                                return Err("Removing one app changed the wrong selection".into());
                            }
                            ui.invoke_back();
                            if ui.get_page() != 0 || ui.get_list_mode()
                                || ui.get_rows().row_data(0).is_none_or(|row| row.first.id != removed || row.first.selected)
                            {
                                return Err("The removal or Grid preference did not carry back to Home".into());
                            }
                            ui.invoke_navigate(INSTALL_QUEUE_PAGE);
                            ui.invoke_action("install-selected".into());
                        }
                        _ => {
                            if !ui.get_confirm_visible() || !ui.get_confirm_text().contains(kept)
                                || ui.get_confirm_text().contains(removed)
                                || !ui.get_confirm_agreement_required() || ui.get_package_consent()
                            {
                                return Err(format!("Install Queue review contains the wrong packages: {}", ui.get_status()));
                            }
                            ui.invoke_select_app(kept.into(), false);
                            ui.invoke_confirmed(false);
                            if ui.get_page() != INSTALL_QUEUE_PAGE || ui.get_install_count() != 1
                                || ui.get_confirm_visible() || ui.get_operation_count() != 0
                            {
                                return Err("Cancelling did not preserve the reviewed selection".into());
                            }
                            ui.invoke_action("clear-selection".into());
                            ui.invoke_action("install-selected".into());
                            if ui.get_install_count() != 0 || ui.get_result_count() != 0
                                || ui.get_busy() || ui.get_confirm_visible() || ui.get_operation_count() != 0
                            {
                                return Err("The empty Install Queue still allowed an install".into());
                            }
                            report("PASS review, details/back, removal, exact preflight, cancellation, and empty queue");
                            return Ok(true);
                        }
                    }
                    phase.set(phase.get() + 1);
                    return Ok(false);
                }
                if mode == "geometry" {
                    match phase.get() {
                        0 => ui.window().set_size(slint::LogicalSize::new(820., 600.)),
                        1 => {
                            if ui.get_columns() != 2 {
                                return Err("820x600 layout failed".into());
                            }
                            ui.window().set_size(slint::LogicalSize::new(1024., 768.));
                        }
                        2 => {
                            if ui.get_columns() != 2 {
                                return Err("1024x768 layout failed".into());
                            }
                            ui.window().set_size(slint::LogicalSize::new(1280., 800.));
                        }
                        3 => {
                            if ui.get_columns() != 3 {
                                return Err("1280x800 layout failed".into());
                            }
                            ui.window().set_size(slint::LogicalSize::new(1600., 900.));
                        }
                        4 => {
                            if ui.get_columns() != 3 {
                                return Err("1600x900 layout failed".into());
                            }
                            ui.invoke_maximize_window();
                        }
                        5 => {
                            if !crate::chrome::maximized(ui.window()) {
                                return Err(
                                    "Native maximize caption did not maximize the window".into()
                                );
                            }
                            ui.invoke_maximize_window();
                        }
                        6 => {
                            if crate::chrome::maximized(ui.window()) {
                                return Err(
                                    "Restore geometry test did not leave maximized state".into()
                                );
                            }
                            report(
                                "PASS 820x600, 1024x768, 1280x800, 1600x900, maximize, and restore",
                            );
                            return Ok(true);
                        }
                        _ => return Ok(true),
                    }
                    phase.set(phase.get() + 1);
                    return Ok(false);
                }
                if mode == "dpi" {
                    if phase.get() == 0 {
                        ui.window().set_size(slint::LogicalSize::new(820., 600.));
                        phase.set(1);
                        return Ok(false);
                    }
                    if ui.get_columns() != 2 {
                        return Err(
                            "820x600 responsive layout failed at the requested display scale"
                                .into(),
                        );
                    }
                    ui.set_list_mode(true);
                    if ui.get_columns() != 1 {
                        return Err("List layout failed at the requested display scale".into());
                    }
                    report("PASS responsive 820x600 and list layout at requested display scale");
                    return Ok(true);
                }
                if matches!(mode.as_str(), "scoop-search" | "scoop-no-winget") {
                    match phase.get() {
                        0 => {
                            if ui.get_busy() { return Ok(false); }
                            ui.set_query("extras/eartrumpet".into());
                            ui.invoke_search("extras/eartrumpet".into());
                            phase.set(1);
                        }
                        1 if ui.get_busy() || ui.get_page() != 8 => return Ok(false),
                        1 if mode == "scoop-no-winget" && ui.get_scoop_search_available() => {
                            if !ui.get_subtitle().contains("unavailable") {
                                return Err("Search did not identify unavailable WinGet".into());
                            }
                            ui.invoke_action("search-scoop".into());
                            phase.set(2);
                        }
                        1 | 2 => {
                            if ui.get_busy() { return Ok(false); }
                            if !ui.get_subtitle().contains("Scoop matches") {
                                return Err(format!("Scoop search did not finish: {}", ui.get_subtitle()));
                            }
                            let row = ui.get_rows().row_data(0).ok_or("No Scoop rows")?;
                            let pkg = [&row.first, &row.second, &row.third].into_iter()
                                .find(|p| p.id == "extras/eartrumpet")
                                .ok_or("Exact Scoop package missing")?;
                            if pkg.source != "scoop" || !pkg.publisher.contains("extras") {
                                return Err("Scoop source label missing".into());
                            }
                            let icon = ui.global::<Artwork>().get_online_icons().row_data(pkg.icon_slot as usize);
                            if icon.is_none_or(|image| image.size().width == 0) {
                                return Ok(false);
                            }
                            report("PASS exact Scoop search result, source label and dynamic project icon");
                            ui.invoke_open_app(pkg.id.clone());
                            phase.set(3);
                        }
                        3 => {
                            if ui.get_busy() { return Ok(false); }
                            if ui.get_detail().id != "extras/eartrumpet" || !ui.get_detail_scoop_result() {
                                return Err("Scoop result did not open Details".into());
                            }
                            report("PASS Scoop details and setup or review path");
                            ui.invoke_action("detect-scoop".into());
                            phase.set(4);
                        }
                        4 => {
                            if ui.get_busy() { return Ok(false); }
                            if ui.get_page() != 7 || ui.get_detail().id != "extras/eartrumpet" {
                                return Err("Checking for Scoop lost the selected package".into());
                            }
                            report("PASS rechecks Scoop without leaving Details");
                            return Ok(true);
                        }
                        _ => return Err("Unexpected Scoop search smoke phase".into()),
                    }
                    return Ok(false);
                }
                match phase.get() {
                    0 => {
                        if ui.get_result_count() != 36 || ui.get_rows().row_count() == 0 {
                            return Err("Home did not show all 36 curated picks".into());
                        }
                        if mode == "live" {
                            let icons = ui.global::<Artwork>().get_online_icons();
                            for id in ["Google.Chrome", "Mozilla.Firefox"] {
                                let slot = crate::icons::slot_for(id) as usize;
                                if icons.row_data(slot).is_none_or(|image| image.size().width == 0) {
                                    if ticks.get() > 40 {
                                        return Err(format!("Home icon did not load for {id}"));
                                    }
                                    return Ok(false);
                                }
                            }
                            report("PASS Home rendered verified WinGet and publisher icons");
                        }
                        report("PASS startup and embedded collection");
                        ui.invoke_navigate(1);
                    }
                    1 => {
                        if ui.get_page() != 1 || ui.get_result_count() < 60 {
                            return Err("Discover navigation failed".into());
                        }
                        report("PASS Discover navigation");
                        ui.invoke_navigate(2);
                        ui.set_category_index(1);
                        ui.invoke_filter(1, 0);
                    }
                    2 => {
                        if ui.get_result_count() < 5 {
                            return Err("Curated browser filtering failed".into());
                        }
                        report("PASS Categories and filtering");
                        ui.set_theme_index(2);
                        ui.invoke_settings_changed();
                    }
                    3 => {
                        if !ui.get_dark() {
                            return Err("Dark theme failed".into());
                        }
                        ui.set_theme_index(1);
                        ui.invoke_settings_changed();
                    }
                    4 => {
                        if ui.get_dark() {
                            return Err("Light theme failed".into());
                        }
                        report("PASS dark and light themes");
                        ui.invoke_navigate(6);
                    }
                    5 => {
                        if ui.get_page() != 6 {
                            return Err("Settings navigation failed".into());
                        }
                        report("PASS Settings navigation");
                        ui.invoke_navigate(5);
                    }
                    6 => {
                        if ui.get_page() != 5 || ui.get_operation_count() != 0 {
                            return Err("Activity navigation failed".into());
                        }
                        report("PASS empty activity and no package changes");
                        ui.invoke_maximize_window();
                    }
                    7 => {
                        if !crate::chrome::maximized(ui.window()) {
                            return Err(
                                "Custom title bar maximize did not maximize the native window"
                                    .into(),
                            );
                        }
                        report("PASS custom title bar maximize");
                        ui.invoke_maximize_window();
                        ui.invoke_navigate(0);
                    }
                    8 => {
                        if crate::chrome::maximized(ui.window()) {
                            return Ok(false);
                        }
                        ui.window().set_size(slint::LogicalSize::new(820., 640.));
                        if ui.get_columns() != 2 {
                            return Ok(false);
                        }
                        if ui.get_result_count() < 8 {
                            return Err("Responsive narrow-window layout failed".into());
                        }
                        report("PASS narrow window and responsive two-column grid");
                        ui.set_list_mode(true);
                    }
                    9 => {
                        if !ui.get_list_mode()
                            || ui.get_columns() != 1
                            || ui.get_rows().row_count() != ui.get_result_count() as usize
                        {
                            return Err(
                                "List view did not retain a uniform package row for every result"
                                    .into(),
                            );
                        }
                        report("PASS virtualized package list view");
                        if mode == "visual" {
                            report("PASS LIST PREVIEW READY");
                            phase.set(8);
                        } else {
                            ui.set_list_mode(false);
                        }
                    }
                    10 => {
                        if ui.get_columns() != 2 || ui.get_list_mode() {
                            return Err("Grid view restore failed".into());
                        }
                        report("PASS grid/list toggle and return to responsive grid");
                        if mode == "live" || mode == "missing" {
                            ui.set_query("Mozilla.Firefox".into());
                            ui.invoke_search("Mozilla.Firefox".into());
                        } else {
                            report("PASS memory-only startup");
                            return Ok(true);
                        }
                    }
                    11 => {
                        if ui.get_busy() || ui.get_page() != 8 {
                            return Ok(false);
                        }
                        if ui.get_result_count() == 0 {
                            return Err(format!("Search returned no apps: {}", ui.get_status()));
                        }
                        if mode == "missing" {
                            if !ui.get_backend_status().contains("unavailable")
                                || !ui.get_subtitle().contains("embedded catalog")
                            {
                                return Err("Embedded search fallback was not shown".into());
                            }
                            report("PASS embedded search fallback with WinGet disabled");
                        } else {
                            report("PASS live debounced WinGet search");
                        }
                        ui.invoke_open_app("Mozilla.Firefox".into());
                    }
                    12 => {
                        if ui.get_busy() {
                            return Ok(false);
                        }
                        if mode == "missing" {
                            if ui.get_detail_ready()
                                || !ui.get_detail_text().contains("Could not verify")
                            {
                                return Err("Missing WinGet did not disable package actions".into());
                            }
                            report("PASS offline Details and disabled package actions");
                            return Ok(true);
                        }
                        if !ui.get_detail_ready()
                            || !ui.get_detail_text().contains("Mozilla.Firefox")
                        {
                            return Err(format!("Live details failed: {}", ui.get_detail_text()));
                        }
                        report("PASS verified package details and installed-state lookup");
                        ui.invoke_navigate(3);
                    }
                    13 => {
                        if ui.get_busy() {
                            return Ok(false);
                        }
                        if !ui.get_inventory_known() {
                            return Err(format!("Installed inventory failed: {}", ui.get_status()));
                        }
                        report(&format!(
                            "PASS live inventory: {} recognized apps",
                            ui.get_result_count()
                        ));
                        ui.invoke_navigate(4);
                    }
                    14 => {
                        if ui.get_busy() {
                            return Ok(false);
                        }
                        if !ui.get_status().contains("updates") {
                            return Err(format!("Update inventory failed: {}", ui.get_status()));
                        }
                        report(&format!(
                            "PASS live update discovery: {} apps",
                            ui.get_result_count()
                        ));
                        ui.set_query("Microsoft".into());
                        ui.invoke_search("Microsoft".into());
                    }
                    15 => {
                        if ui.get_busy() || ui.get_page() != 8 {
                            return Ok(false);
                        }
                        if ui.get_result_count() < 60 {
                            return Err(format!(
                                "Broad search returned only {} apps",
                                ui.get_result_count()
                            ));
                        }
                        report(&format!(
                            "PASS broad WinGet search: {} apps",
                            ui.get_result_count()
                        ));
                        return Ok(true);
                    }
                    _ => return Ok(true),
                }
                phase.set(phase.get() + 1);
                Ok(false)
            })();
            match result {
                Ok(true) => {
                    report("PASS GUI SMOKE COMPLETE");
                    let _ = slint::quit_event_loop();
                }
                Err(error) => {
                    report(&format!("FAIL {error}"));
                    std::process::exit(2);
                }
                _ => {}
            }
        },
    );
    timer
}
