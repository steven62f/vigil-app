use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use crate::agent::state::AgentState;
use crate::agent::state_engine::AgentStateEngine;

pub struct PidWatcher;

impl PidWatcher {
    pub fn start(app: AppHandle, engine: Arc<Mutex<AgentStateEngine>>) {
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                let watched: Vec<(String, u32)> = {
                    let eng = engine.lock().unwrap();
                    eng.agents
                        .values()
                        .filter(|a| should_watch_for_liveness(&a.state))
                        .filter_map(|a| a.terminal_pid.map(|pid| (a.transcript_path.clone(), pid)))
                        .collect()
                };

                for (transcript_path, pid) in watched {
                    if !is_process_alive(pid) {
                        let mut eng = engine.lock().unwrap();
                        eng.set_disconnected(&transcript_path);
                        let _ = eng.emit_snapshot(&app);
                    }
                }
            }
        });
    }
}

/// Un agent doit être surveillé pour la liveness de son terminal tant qu'il n'est PAS déjà
/// déconnecté. On surveille donc AUSSI `Ended`/`Error` (tour terminé mais session/terminal encore
/// vivants) : sinon un agent qui a émis `Stop` (→ `Ended`) ne serait jamais nettoyé à la fermeture
/// de sa fenêtre (bug #2/#3 Codex/Antigravity, Story 6.20). Seul `Disconnected` est exclu (déjà fait).
fn should_watch_for_liveness(state: &AgentState) -> bool {
    !matches!(state, AgentState::Disconnected)
}

fn is_process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // ERROR_INVALID_PARAMETER (0x80070057) : le PID n'existe pas → process mort
    // ERROR_ACCESS_DENIED   (0x80070005) : process vivant, pas de permission → supposer vivant
    const HRESULT_INVALID_PARAMETER: i32 = 0x80070057u32 as i32;

    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) if e.code().0 == HRESULT_INVALID_PARAMETER => return false,
            Err(_) => return true, // autre erreur (ex: accès refusé) → supposer vivant
        };
        let mut exit_code: u32 = 0;
        let confirmed_dead = GetExitCodeProcess(handle, &mut exit_code).is_ok()
            && exit_code != 259; // 259 = STILL_ACTIVE
        let _ = CloseHandle(handle);
        !confirmed_dead
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_tous_les_etats_sauf_disconnected() {
        // Bug #2/#3 (Story 6.20) : `Ended`/`Error` DOIVENT être surveillés — un agent Codex/
        // Antigravity qui a émis `Stop` (→ `Ended`) et n'a pas de `SessionEnd` ne serait sinon
        // jamais nettoyé à la fermeture de sa fenêtre.
        assert!(should_watch_for_liveness(&AgentState::Idle));
        assert!(should_watch_for_liveness(&AgentState::Working));
        assert!(should_watch_for_liveness(&AgentState::WaitingApprove));
        assert!(should_watch_for_liveness(&AgentState::WaitingSpecific));
        assert!(should_watch_for_liveness(&AgentState::Ended));
        assert!(should_watch_for_liveness(&AgentState::Error));
        // Déjà déconnecté → inutile de surveiller.
        assert!(!should_watch_for_liveness(&AgentState::Disconnected));
    }
}
