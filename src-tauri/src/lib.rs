mod commands;
mod error;
mod git;
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
use tracing::{error, info};

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
                // Schema is nice-to-have (editor autocomplete); don't block boot.
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

            let state_path = paths.state_file();
            let tmp_path = paths.state_tmp_file();

            tauri::async_runtime::block_on(async {
                match Store::load(state_path, tmp_path).await {
                    Ok(store) => {
                        handle.manage::<Arc<Store>>(store);
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    Err(e) => {
                        error!(error = %e, "failed to load store");
                        Err(Box::new(e) as Box<dyn std::error::Error>)
                    }
                }
            })?;

            app.manage(paths);
            app.manage::<Arc<SessionSupervisor>>(Arc::new(SessionSupervisor::new(
                handle.clone(),
            )));
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
            commands::start_session,
            commands::attach_session,
            commands::send_input,
            commands::resize_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

struct LoggingGuard(#[allow(dead_code)] tracing_appender::non_blocking::WorkerGuard);
