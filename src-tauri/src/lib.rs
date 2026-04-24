mod claude;
mod commands;
mod error;
mod git;
mod github;
mod hook_install;
mod hook_listener;
mod inprogress;
mod job;
mod logging;
mod paths;
mod reconcile;
mod registry;
mod sessions;
mod setup;
mod state;
mod store;
mod theme;

use std::sync::Arc;

use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::{Manager, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tracing::{error, info, warn};

use crate::commands::ClaudeBin;
use crate::github::GithubPoller;
use crate::paths::Paths;
use crate::registry::RegistryLoad;
use crate::sessions::SessionSupervisor;
use crate::store::Store;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            let handle = app.handle().clone();
            let paths = Paths::from_app(&handle)?;

            let guard = logging::init(&paths.logs_dir());
            app.manage(LoggingGuard(guard));

            info!(data_dir = ?paths.data_dir, "tethys starting up");

            if let Err(e) = registry::write_schema(&paths.repos_schema_file()) {
                error!(error = %e, "failed to write repos.schema.json");
            }

            let registry_load = RegistryLoad::load(&paths.repos_config_file());
            match &registry_load {
                RegistryLoad::Ok { .. } => info!("registry ok"),
                RegistryLoad::Missing { path } => {
                    info!(?path, "repos.toml missing — user will be prompted to create it")
                }
                RegistryLoad::Invalid { path, error } => {
                    error!(?path, %error, "repos.toml failed to load")
                }
            }
            app.manage::<Arc<RegistryLoad>>(Arc::new(registry_load));

            // --- state store ------------------------------------------------
            let state_path = paths.state_file();
            let tmp_path = paths.state_tmp_file();
            let store: Arc<Store> = tauri::async_runtime::block_on(async {
                Store::load(state_path, tmp_path).await
            })
            .map_err(|e| {
                error!(error = %e, "failed to load store");
                Box::new(e) as Box<dyn std::error::Error>
            })?;
            handle.manage::<Arc<Store>>(store.clone());

            // --- claude binary (non-fatal if missing; surface to UI later) --
            match claude::resolve() {
                Ok(path) => {
                    app.manage(ClaudeBin(path));
                }
                Err(e) => {
                    warn!(error = %e, "claude binary not resolved at startup");
                    // Still manage a placeholder so commands can surface
                    // the error at spawn time rather than panicking.
                    app.manage(ClaudeBin(std::path::PathBuf::new()));
                }
            }

            // --- hook installer (idempotent) --------------------------------
            if let Some(hook_bin) = hook_install::bundled_hook_bin_or_warn() {
                if let Some(settings) = paths::claude_settings_path() {
                    if let Err(e) = hook_install::install(
                        &settings,
                        &paths.claude_settings_lock(),
                        &hook_bin,
                    ) {
                        warn!(error = %e, "hook install failed");
                    }
                } else {
                    warn!("HOME not set; skipping hook install");
                }
            }

            // --- session supervisor + UDS listener --------------------------
            let supervisor: Arc<SessionSupervisor> =
                Arc::new(SessionSupervisor::new(handle.clone(), store.clone()));
            app.manage(supervisor.clone());

            let socket_path = paths.hook_socket();
            let sup_for_listener = supervisor.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = hook_listener::start(&socket_path, sup_for_listener).await {
                    error!(error = %e, "hook listener failed to start");
                }
            });

            // --- github poller ---------------------------------------------
            let registry_for_poller: Arc<RegistryLoad> = app.state::<Arc<RegistryLoad>>().inner().clone();
            let poller = Arc::new(GithubPoller::new(
                store.clone(),
                registry_for_poller,
                handle.clone(),
            ));
            app.manage(poller.clone());
            let poller_for_probe = poller.clone();
            tauri::async_runtime::spawn(async move {
                match poller_for_probe.probe_login().await {
                    Some(login) => info!(login, "gh authenticated"),
                    None => info!("gh auth probe failed — polling will retry"),
                }
            });
            tauri::async_runtime::spawn(poller.clone().run());

            app.manage(paths);
            app.manage(inprogress::InProgressWorkspaces::new());

            // --- menu (append Theme items under the default View submenu) --
            if let Err(e) = install_menu(&handle) {
                warn!(error = %e, "menu install failed");
            }
            handle.on_menu_event(move |app, event| match event.id().as_ref() {
                "theme_load" => handle_theme_load(app),
                "theme_reset" => handle_theme_reset(app),
                _ => {}
            });

            // --- window focus → force-tick the github poller ---------------
            if let Some(window) = app.get_webview_window("main") {
                let poller_for_focus = poller.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::Focused(true) = event {
                        let p = poller_for_focus.clone();
                        tauri::async_runtime::spawn(async move {
                            p.request_tick().await;
                        });
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_workspaces,
            commands::get_workspace,
            commands::create_workspace,
            commands::delete_workspace,
            commands::pause_workspace,
            commands::resume_workspace,
            commands::list_repos,
            commands::registry_status,
            commands::github_auth_status,
            commands::github_reprobe_auth,
            commands::open_repos_config,
            commands::list_discrepancies,
            commands::remove_orphan_dir,
            commands::forget_workspace,
            commands::list_sessions,
            commands::start_claude_session,
            commands::resume_claude_session,
            commands::attach_session,
            commands::send_input,
            commands::resize_session,
            commands::get_theme,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

struct LoggingGuard(#[allow(dead_code)] tracing_appender::non_blocking::WorkerGuard);

/// Build the default OS menu, then append Theme items under the View submenu.
/// If the layout of the default menu changes upstream and "View" isn't found,
/// the items are tucked into the app-name submenu as a fallback.
fn install_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    let menu = Menu::default(app)?;
    let load = MenuItem::with_id(app, "theme_load", "Load .itermcolors…", true, None::<&str>)?;
    let reset = MenuItem::with_id(app, "theme_reset", "Reset theme to default", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let items: &[&dyn IsMenuItem<_>] = &[&sep, &load, &reset];

    let mut appended = false;
    for kind in menu.items()? {
        if let Some(sub) = kind.as_submenu() {
            if sub.text().unwrap_or_default() == "View" {
                sub.append_items(items)?;
                appended = true;
                break;
            }
        }
    }
    if !appended {
        warn!("View submenu not found; theme items not installed");
    }
    menu.set_as_app_menu()?;
    Ok(())
}

fn handle_theme_load(app: &tauri::AppHandle) {
    let app = app.clone();
    app.clone()
        .dialog()
        .file()
        .add_filter("iTerm2 colors", &["itermcolors"])
        .pick_file(move |picked| {
            let Some(picked) = picked else { return };
            let Ok(source) = picked.into_path() else { return };
            let save_path = app.state::<Paths>().theme_file();
            if let Err(e) = theme::load_and_emit(&app, &source, &save_path) {
                error!(error = %e, source = %source.display(), "load theme failed");
            }
        });
}

fn handle_theme_reset(app: &tauri::AppHandle) {
    let save_path = app.state::<Paths>().theme_file();
    if let Err(e) = theme::clear_and_emit(app, &save_path) {
        error!(error = %e, "clear theme failed");
    }
}
