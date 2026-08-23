use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use allen_runtime::ExternalGrantId;
use allen_vm::VmError;

#[derive(Default)]
pub(crate) struct GrantRegistry {
    state: Mutex<GrantRegistryState>,
}

#[derive(Default)]
struct GrantRegistryState {
    pending_host_ids: BTreeMap<u64, String>,
    revoked_pending_targets: BTreeSet<u64>,
    runtime_ids: BTreeMap<String, ExternalGrantId>,
    revoked: Vec<ExternalGrantId>,
}

impl GrantRegistry {
    pub(crate) fn allowed(
        &self,
        pending_target_id: u64,
        host_grant_id: String,
    ) -> Result<(), VmError> {
        let mut state = self.state.lock().map_err(|_| VmError::AgentUnavailable)?;
        if state
            .pending_host_ids
            .insert(pending_target_id, host_grant_id)
            .is_some()
        {
            return Err(VmError::AgentUnavailable);
        }
        Ok(())
    }

    pub(crate) fn issued(&self, pending_target_id: u64, runtime_id: ExternalGrantId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.revoked_pending_targets.remove(&pending_target_id) {
            state.revoked.push(runtime_id);
        } else if let Some(host_id) = state.pending_host_ids.remove(&pending_target_id) {
            state.runtime_ids.insert(host_id, runtime_id);
        }
    }

    pub(crate) fn revoke(&self, host_grant_id: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(runtime_id) = state.runtime_ids.remove(host_grant_id) {
            state.revoked.push(runtime_id);
            return;
        }
        let pending_target = state
            .pending_host_ids
            .iter()
            .find_map(|(pending, host_id)| (host_id == host_grant_id).then_some(*pending));
        if let Some(pending_target) = pending_target {
            state.pending_host_ids.remove(&pending_target);
            state.revoked_pending_targets.insert(pending_target);
        }
    }

    pub(crate) fn take_revocations(&self) -> Result<Vec<ExternalGrantId>, VmError> {
        let mut state = self.state.lock().map_err(|_| VmError::AgentUnavailable)?;
        Ok(std::mem::take(&mut state.revoked))
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = GrantRegistryState::default();
        }
    }
}
