use crate::account::types::Account;
use crate::utils::now_ms;

const RUNTIME_UNSUPPORTED_TTL_MS: u64 = 10 * 60 * 1000;

pub fn is_model_supported_by_account(account: &Account, model_id: &str) -> bool {
    if model_id.is_empty() {
        return true;
    }

    if let Some(&marked_at) = account.unsupported_models.get(model_id) {
        if now_ms().saturating_sub(marked_at) < RUNTIME_UNSUPPORTED_TTL_MS {
            return false;
        }
    }

    if !account.supported_models.is_empty() {
        return account.supported_models.contains(model_id);
    }

    if !account.quota.models.is_empty() {
        return account.quota.models.contains_key(model_id);
    }

    true
}

pub fn mark_account_model_unsupported(account: &mut Account, model_id: &str) {
    if !model_id.is_empty() {
        account
            .unsupported_models
            .insert(model_id.to_string(), now_ms());
    }
}

pub fn clear_account_model_unsupported(account: &mut Account, model_id: &str) {
    account.unsupported_models.remove(model_id);
}
