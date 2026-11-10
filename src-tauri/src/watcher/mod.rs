use std::sync::Mutex;
use tokio::task::JoinHandle;

/// État partagé pour le watcher de conversation (polling FSW 2s).
/// Stocké dans le state Tauri via `.manage(ConversationWatcher::new_state())`.
pub struct ConversationWatcher;

impl ConversationWatcher {
    pub fn new_state() -> Mutex<Option<JoinHandle<()>>> {
        Mutex::new(None)
    }
}
