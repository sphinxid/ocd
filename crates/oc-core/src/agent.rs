//! Agents (`core/src/agent.ts`, `core/src/plugin/agent.ts`, `core/src/plugin/plan.ts`).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use oc_schema::types::{Effect, ModelRef, PermissionRule, Ruleset};

pub const DEFAULT_ID: &str = "build";
pub const PROMPT_EXPLORE: &str = include_str!("../../../assets/prompt/agent/explore.txt");
pub const PROMPT_TITLE: &str = include_str!("../../../assets/prompt/agent/title.txt");
pub const PROMPT_SUMMARY: &str = include_str!("../../../assets/prompt/agent/summary.txt");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Subagent,
    Primary,
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub settings: Map<String, Value>,
    pub headers: Map<String, Value>,
    pub body: Map<String, Value>,
}

/// `Agent.Info`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub request: Request,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: Mode,
    pub hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    pub permissions: Ruleset,
}

fn rule(action: &str, resource: &str, effect: Effect) -> PermissionRule {
    PermissionRule { action: action.into(), resource: resource.into(), effect }
}

impl Agent {
    /// `Info.default(id)`
    pub fn default_for(id: &str) -> Agent {
        Agent {
            id: id.into(),
            name: id.into(),
            model: None,
            request: Request::default(),
            system: None,
            description: None,
            mode: Mode::Primary,
            hidden: false,
            color: None,
            steps: None,
            permissions: vec![
                rule("*", "*", Effect::Allow),
                rule("read", "*.env", Effect::Ask),
                rule("read", "*.env.*", Effect::Ask),
                rule("read", "*.env.example", Effect::Allow),
            ],
        }
    }

    pub fn selectable(&self) -> bool {
        self.mode != Mode::Subagent && !self.hidden
    }
}

#[derive(Debug, Clone, Default)]
pub struct Agents {
    pub items: IndexMap<String, Agent>,
    pub default: Option<String>,
}

pub fn plan_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".opencode").join("plan")
}

impl Agents {
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut Agent)) {
        let entry = self.items.entry(id.to_string()).or_insert_with(|| Agent::default_for(id));
        f(entry);
    }

    /// Built-in agents (AgentPlugin then PlanPlugin order).
    pub fn builtin(home: &std::path::Path) -> Agents {
        use Effect::*;
        let mut a = Agents::default();
        a.update(DEFAULT_ID, |i| {
            i.name = "Build".into();
            i.description = Some("The default agent. Executes tools based on configured permissions.".into());
            i.mode = Mode::Primary;
            i.permissions.push(rule("question", "*", Allow));
        });
        a.update("general", |i| {
            i.name = "General".into();
            i.description = Some("General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.".into());
            i.mode = Mode::Subagent;
            i.permissions.extend([rule("question", "*", Deny), rule("subagent", "*", Deny)]);
        });
        a.update("explore", |i| {
            i.name = "Explore".into();
            i.description = Some("Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".into());
            i.system = Some(PROMPT_EXPLORE.into());
            i.mode = Mode::Subagent;
            i.permissions.extend([
                rule("*", "*", Deny),
                rule("shell", "*", Allow),
                rule("grep", "*", Allow),
                rule("glob", "*", Allow),
                rule("webfetch", "*", Allow),
                rule("websearch", "*", Allow),
                rule("read", "*", Allow),
                rule("read", "*.env", Ask),
                rule("read", "*.env.*", Ask),
                rule("read", "*.env.example", Allow),
                rule("subagent", "*", Deny),
                rule("external_directory", "*", Allow),
            ]);
        });
        a.update("compaction", |i| {
            i.name = "Compaction".into();
            i.mode = Mode::Primary;
            i.hidden = true;
        });
        a.update("title", |i| {
            i.name = "Title".into();
            i.mode = Mode::Primary;
            i.hidden = true;
            i.system = Some(PROMPT_TITLE.into());
            i.permissions.push(rule("*", "*", Deny));
        });
        a.update("summary", |i| {
            i.name = "Summary".into();
            i.mode = Mode::Primary;
            i.hidden = true;
            i.system = Some(PROMPT_SUMMARY.into());
            i.permissions.push(rule("*", "*", Deny));
        });
        let plan = plan_dir(home);
        a.update("plan", |i| {
            i.name = "Plan".into();
            i.description = Some("Read-only agent for exploring the codebase and planning work before implementation.".into());
            i.mode = Mode::Primary;
            i.permissions.extend([
                rule("question", "*", Allow),
                rule("edit", "*", Deny),
                rule("edit", &format!("{}/*", plan.display()), Allow),
            ]);
        });
        a
    }

    /// Default agent id (`default_agent` if selectable, else build if selectable, else first selectable).
    pub fn default_id(&self) -> Option<String> {
        let ok = |id: &str| self.items.get(id).is_some_and(Agent::selectable);
        if let Some(d) = self.default.as_deref().filter(|d| ok(d)) {
            return Some(d.into());
        }
        if ok(DEFAULT_ID) {
            return Some(DEFAULT_ID.into());
        }
        self.items.values().find(|a| a.selectable()).map(|a| a.id.clone())
    }

    pub fn select(&self, id: Option<&str>) -> Option<&Agent> {
        match id {
            Some(id) => self.items.get(id),
            None => self.items.get(&self.default_id().unwrap_or_else(|| DEFAULT_ID.into())),
        }
    }

    /// `list()` puts the default first.
    pub fn list(&self) -> Vec<&Agent> {
        let d = self.default_id();
        let mut out: Vec<&Agent> = self.items.values().collect();
        if let Some(d) = d {
            if let Some(pos) = out.iter().position(|a| a.id == d) {
                let item = out.remove(pos);
                out.insert(0, item);
            }
        }
        out
    }
}

pub const PLAN_ENTER: &str = "<system-reminder>\nYou are in Plan mode. Discuss the plan with the user directly in the conversation. Do not create or update plan files unless the user explicitly asks you to; when they do, write them only in:\n${directory}\n\nDo not modify any other files or ask a subagent to do so.\n\nYou remain in Plan mode until the user switches agents. If the user asks you to implement changes, do not do so. Tell them they need to switch agents.\n</system-reminder>";
pub const PLAN_LEAVE: &str = "<system-reminder>\nYou are NO LONGER in Plan mode. The previous Plan restrictions no longer apply. Any Plan mode instructions from earlier in this conversation are no longer active.\n</system-reminder>";

pub fn plan_enter(home: &std::path::Path) -> String {
    PLAN_ENTER.replace("${directory}", &plan_dir(home).display().to_string())
}
