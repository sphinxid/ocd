"""All scenarios. Each function runs against one binary through `ctx` (see harness.Ctx); keep them deterministic."""
from parity import loop, sessions, readonly, errors, cli  # noqa: F401  (registration side effects)
