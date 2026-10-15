use crate::provider::UsageScript;
use crate::services::pi_state::{PiCurrentState, PiStateService};
use crate::services::ProviderService;
use crate::session_manager::providers::pi::PiSessionDiscovery;
use crate::store::AppState;
use tauri::State;

#[tauri::command]
pub(crate) fn get_pi_observation_status(
    state: State<'_, AppState>,
) -> Result<crate::services::pi_observation::PiObservationStatus, String> {
    crate::services::pi_observation::status(&state.db).map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn set_pi_observation_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<crate::services::pi_observation::PiObservationStatus, String> {
    crate::services::pi_observation::set_enabled(&state.db, enabled)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn get_pi_current_state(state: State<'_, AppState>) -> Result<PiCurrentState, String> {
    PiStateService::current(state.inner()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn update_pi_provider_usage_script(
    state: State<'_, AppState>,
    id: String,
    #[allow(non_snake_case)] usageScript: UsageScript,
) -> Result<bool, String> {
    ProviderService::update_pi_usage_script(state.inner(), &id, usageScript)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn get_pi_session_discovery() -> PiSessionDiscovery {
    crate::session_manager::providers::pi::session_discovery()
}
