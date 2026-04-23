mod claude;
mod commands;
mod error;
mod git;
mod hook_install;
mod hook_listener;
mod job;
mod logging;
mod paths;
mod reconcile;
mod registry;
mod sessions;
mod setup;
mod state;
mod store;

use std::sync::Arc;

use tauri::Manager;
use tracing::{error, info, warn};

use crate::commands::ClaudeBin;
use crate::paths::Paths;
use crate::registry::RegistryLoad;
use crate::sessions::SessionSupervisor;
use crate::store::Store;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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

            app.manage(paths);
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

struct LoggingGuard(#[allow(dead_code)] tracing_appender::non_blocking::WorkerGuard);
