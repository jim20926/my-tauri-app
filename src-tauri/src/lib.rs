mod crypto;
mod download;
mod logging;
mod playback;
mod watch_progress;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    log::info!(target: "app", "application starting");
    let playback_state = playback::PlaybackState::default();
    let playback_server = playback::PlaybackServer::start(playback_state.clone())
        .expect("error while starting playback HTTP server");
    tauri::Builder::default()
        .manage(playback_state)
        .manage(playback_server)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            download::download_video,
            download::list_downloads,
            download::delete_download,
            playback::open_playback,
            playback::close_playback,
            watch_progress::list_watch_progress,
            watch_progress::save_watch_progress,
            watch_progress::mark_watch_progress_synced,
            watch_progress::apply_remote_watch_progress,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
