"""Scenario registry: `@scenario(group)` registers a function `fn(ctx)` under its name."""
SCENARIOS = {}


class Scenario:
    def __init__(self, fn, group, hold_for_title, timeout):
        self.fn = fn
        self.name = fn.__name__
        self.group = group
        self.hold_for_title = hold_for_title
        self.timeout = timeout
        self.doc = (fn.__doc__ or "").strip().splitlines()[0] if fn.__doc__ else ""


def scenario(group, hold_for_title=5.0, timeout=180):
    def deco(fn):
        SCENARIOS[fn.__name__] = Scenario(fn, group, hold_for_title, timeout)
        return fn
    return deco
