//! The runtime half of the built-in OpenAI provider plugins (`opencode.provider.openai`,
//! `opencode.provider.chatgpt`, `opencode.provider.openai-context-caps`); the catalog transforms are
//! in `oc_ai::catalog::openai`.
//!
//! - **Loaded state** per location: how the `openai` integration's active connection signs in. It is
//!   loaded when the location's catalog is built and again when the active connection changes
//!   (upstream: when the plugin activates and on every `credential.switched` for the integration).
//! - **`session.context` / `session.compaction`** (OpenAI plugin, any connection): the default output
//!   limit is dropped for provider `openai`; the ChatGPT backend rejects one, and OpenAI counts one
//!   against rate limits.
//! - **`session.model.request`**: Codex requests go to the Codex base URL with `originator` and
//!   `session-id`; token-sharing requests carry `session-id`, `thread-id` and `x-client-request-id`.
//! - **`session.retry`** (token sharing): sharing failures that will not clear by themselves are not
//!   retried.

use std::collections::HashMap;

use oc_ai::catalog::openai::SignIn;
use parking_lot::Mutex;

#[derive(Default)]
pub struct OpenAi {
    states: Mutex<HashMap<String, SignIn>>,
}

impl OpenAi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the sign-in state location `key`'s catalog was built with.
    pub fn set(&self, key: &str, state: SignIn) {
        self.states.lock().insert(key.to_string(), state);
    }

    /// The loaded sign-in state of location `key` ([`SignIn::None`] before its catalog is built).
    pub fn get(&self, key: &str) -> SignIn {
        self.states.lock().get(key).cloned().unwrap_or_default()
    }

    /// Forgets a closed location.
    pub fn remove(&self, key: &str) {
        self.states.lock().remove(key);
    }
}
