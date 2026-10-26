//! Ports of `packages/core/test/config/{config,watch,agent,command,skill}.test.ts` behaviors that do
//! not depend on the Effect runtime (discovery, load order, Markdown sources, folds).

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use oc_config::discovery::{Options, ProjectRoot, Sources, WatchTarget, discover, plan};
use oc_config::fold;
use oc_config::load::{Loader, update_shell};
use oc_config::schema::{Entry, Info};
use oc_config::sources;
use serde_json::json;

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let dir = std::env::temp_dir().join(format!("oc-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(std::fs::canonicalize(&dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }
    fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn loader(_tmp: &Tmp, directory: &Path, global: &Path, options: Options) -> Loader {
    Loader {
        global_config: global.to_path_buf(),
        home: global.join("home"),
        directory: directory.to_path_buf(),
        options,
        wellknown: None,
    }
}

fn in_fixture(tmp: &Tmp, entry: &Entry) -> bool {
    entry.path().is_none_or(|p| p.starts_with(tmp.path()))
}

fn schema_or_path(entry: &Entry) -> String {
    match entry {
        Entry::Document { info, .. } => info.schema.clone().unwrap_or_default(),
        Entry::Directory { path } => path.to_string_lossy().to_string(),
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
}

#[test]
fn loads_global_and_ancestor_configuration_across_the_project_boundary() {
    let tmp = Tmp::new();
    let global = tmp.mkdir("global");
    let root = tmp.path().join("repo");
    let parent = root.join("packages");
    let directory = parent.join("app");
    let global_agents = tmp.mkdir("global/home/.agents");
    let global_claude = tmp.mkdir("global/home/.claude");
    std::fs::create_dir_all(&directory).unwrap();
    for d in [".agents", ".claude", ".opencode"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
        std::fs::create_dir_all(directory.join(d)).unwrap();
    }
    tmp.write("opencode.json", r#"{"$schema":"outside"}"#);
    tmp.write("global/opencode.json", r#"{"$schema":"global"}"#);
    tmp.write("repo/opencode.json", r#"{"$schema":"root"}"#);
    tmp.write("repo/packages/opencode.jsonc", r#"{"$schema":"parent"}"#);
    tmp.write("repo/packages/app/opencode.json", r#"{"$schema":"directory"}"#);
    tmp.write("repo/.opencode/opencode.json", r#"{"$schema":"root-dot"}"#);
    tmp.write("repo/packages/app/.opencode/opencode.jsonc", r#"{"$schema":"directory-dot"}"#);

    let l = loader(&tmp, &directory, &global, Options::default());
    let (sources, entries) = block_on(l.entries()).unwrap();
    let entries: Vec<_> = entries.into_iter().filter(|e| in_fixture(&tmp, e)).collect();
    let rendered: Vec<String> = entries.iter().map(schema_or_path).collect();
    let s = |p: &Path| p.to_string_lossy().to_string();
    assert_eq!(rendered, vec![
        "global".to_string(),
        s(&global),
        "outside".into(),
        "root".into(),
        "parent".into(),
        "directory".into(),
        "root-dot".into(),
        s(&root.join(".opencode")),
        "directory-dot".into(),
        s(&directory.join(".opencode")),
    ]);
    let (claude, agents) = sources.compatibility();
    let fixture = |v: Vec<PathBuf>| v.into_iter().filter(|p| p.starts_with(tmp.path())).collect::<Vec<_>>();
    assert_eq!(fixture(agents), vec![global_agents, root.join(".agents"), directory.join(".agents")]);
    assert_eq!(fixture(claude), vec![global_claude, root.join(".claude"), directory.join(".claude")]);
}

#[test]
fn loads_json_then_jsonc_and_caches_nothing_itself() {
    let tmp = Tmp::new();
    let global = tmp.mkdir("global");
    tmp.write("opencode.json", r#"{"$schema":"base","providers":{"base":{"name":"Base"}}}"#);
    tmp.write(
        "opencode.jsonc",
        "{\n  // Later files override scalar fields while retaining providers.\n  \"$schema\": \"last\",\n  \"providers\": { \"last\": { \"name\": \"Last\" } },\n}",
    );
    let l = loader(&tmp, tmp.path(), &global, Options::default());
    let (_, entries) = block_on(l.entries()).unwrap();
    let docs: Vec<&Entry> = entries.iter().filter(|e| matches!(e, Entry::Document { .. }) && in_fixture(&tmp, e)).collect();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs.iter().map(|d| schema_or_path(d)).collect::<Vec<_>>(), vec!["base", "last"]);
    assert_eq!(docs[0].path().unwrap(), &tmp.path().join("opencode.json"));
    assert_eq!(fold::latest(&entries, |i| i.schema.as_ref()).map(String::as_str), Some("last"));
}

#[test]
fn returns_only_the_global_directory_when_no_files_exist() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    let l = loader(&tmp, tmp.path(), &global, Options::default());
    let (_, entries) = block_on(l.entries()).unwrap();
    let entries: Vec<_> = entries.into_iter().filter(|e| in_fixture(&tmp, e)).collect();
    assert_eq!(entries, vec![Entry::Directory { path: global }]);
}

#[test]
fn substitutes_environment_variables_and_relative_file_contents() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    let var = format!("OC_CONFIG_TEST_TOKEN_{}", std::process::id());
    // SAFETY: unique variable name, test-local.
    unsafe { std::env::set_var(&var, "secret") };
    tmp.write("token.txt", "file\n\"token\"\n");
    tmp.write(
        "opencode.jsonc",
        &format!(
            r#"{{
  // Ignored reference: {{file:missing.txt}}
  "username": "user-{{env:OC_CONFIG_TEST_MISSING_VAR}}",
  "mcp": {{ "servers": {{ "remote": {{
    "type": "remote", "url": "https://example.com/mcp",
    "headers": {{ "Authorization": "Bearer {{env:{var}}}", "X-Token": "{{file:token.txt}}" }}
  }} }} }}
}}"#
        ),
    );
    let l = loader(&tmp, tmp.path(), &global, Options::default());
    let (_, entries) = block_on(l.entries()).unwrap();
    let info = entries.iter().filter(|e| in_fixture(&tmp, e)).find_map(Entry::info).unwrap();
    assert_eq!(info.username.as_deref(), Some("user-"));
    let encoded = info.to_value();
    assert_eq!(
        encoded["mcp"]["servers"]["remote"]["headers"],
        json!({ "Authorization": "Bearer secret", "X-Token": "file\n\"token\"" })
    );
}

#[test]
fn missing_file_reference_is_fatal_with_verbatim_message() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    let file = tmp.write("opencode.json", r#"{"username":"{file:nope.txt}"}"#);
    let l = loader(&tmp, tmp.path(), &global, Options::default());
    let err = block_on(l.entries()).unwrap_err();
    let named = err.to_named_error();
    assert_eq!(named["name"], "ConfigInvalidError");
    assert_eq!(named["data"]["path"], json!(file.to_string_lossy()));
    assert_eq!(
        named["data"]["message"],
        json!(format!("bad file reference: \"{{file:nope.txt}}\" {} does not exist", tmp.path().join("nope.txt").display()))
    );
}

#[test]
fn does_not_load_legacy_config_json_and_ignores_invalid_files() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    tmp.write("config.json", r#"{"$schema":"legacy"}"#);
    tmp.write("opencode.json", r#"{"$schema":"base"}"#);
    tmp.write("opencode.jsonc", "{ invalid");
    let l = loader(&tmp, tmp.path(), &global, Options::default());
    let (sources, entries) = block_on(l.entries()).unwrap();
    let docs: Vec<String> =
        entries.iter().filter(|e| in_fixture(&tmp, e) && e.info().is_some()).map(schema_or_path).collect();
    assert_eq!(docs, vec!["base"]);
    assert!(plan(&sources).contains(&WatchTarget::Entries {
        path: tmp.path().to_path_buf(),
        kind: oc_config::discovery::EntriesKind::Entries,
        names: vec![".agents", ".claude", ".opencode", "opencode.json", "opencode.jsonc"]
            .into_iter()
            .map(String::from)
            .collect(),
    }));
}

#[test]
fn explicit_file_ranks_below_project_files_and_content_is_highest() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    let project = tmp.mkdir("project");
    tmp.write("project/opencode.json", r#"{"shell":"project"}"#);
    let explicit = tmp.write("custom/explicit.json", r#"{"shell":"explicit","username":"explicit"}"#);
    let options = Options {
        file: Some(explicit.to_string_lossy().to_string()),
        content: Some(r#"{"default_agent":"content"}"#.into()),
        ..Default::default()
    };
    let l = loader(&tmp, &project, &global, options);
    let (_, entries) = block_on(l.entries()).unwrap();
    assert_eq!(fold::shell(&entries), Some("project"));
    assert_eq!(fold::latest(&entries, |i| i.username.as_ref()).map(String::as_str), Some("explicit"));
    assert_eq!(fold::default_agent(&entries), Some("content"));
    let last = entries.last().unwrap();
    assert!(matches!(last, Entry::Document { path: None, .. }));
}

#[test]
fn project_false_skips_the_walk_and_global_false_skips_global() {
    let tmp = Tmp::new();
    let global = tmp.mkdir("global");
    tmp.write("global/opencode.json", r#"{"shell":"global"}"#);
    tmp.write("opencode.json", r#"{"shell":"project"}"#);
    let l = loader(&tmp, tmp.path(), &global, Options { project: Some(false), ..Default::default() });
    let (sources, entries) = block_on(l.entries()).unwrap();
    assert!(sources.direct.is_empty());
    assert_eq!(fold::shell(&entries), Some("global"));
    let l = loader(&tmp, tmp.path(), &global, Options { global: Some(false), ..Default::default() });
    let (sources, entries) = block_on(l.entries()).unwrap();
    assert!(sources.global.is_none());
    assert_eq!(fold::shell(&entries), Some("project"));
    assert!(sources.claude.iter().all(|p| !p.starts_with(global.join("home"))));
}

#[test]
fn deduplicates_global_ecosystem_directories_found_during_upward_discovery() {
    let tmp = Tmp::new();
    let global = tmp.path().join("global");
    let home = global.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".agents")).unwrap();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let sources = discover(&global, &home, &project, &Options::default());
    let (claude, agents) = sources.compatibility();
    assert_eq!(claude, vec![home.join(".claude")]);
    assert_eq!(agents, vec![home.join(".agents")]);
}

#[test]
fn global_directory_as_location_skips_the_walk() {
    let tmp = Tmp::new();
    let global = tmp.mkdir("global");
    let sources = discover(&global, &global.join("home"), &global, &Options::default());
    assert!(sources.direct.is_empty() && sources.project.is_empty());
}

// --- watch.test.ts -------------------------------------------------------------------------------

fn watch_sources() -> (PathBuf, PathBuf, Sources) {
    let project = PathBuf::from("/watch-plan-project");
    let root = project.join(".opencode");
    let sources = Sources {
        global: None,
        explicit: None,
        direct: vec![project.join("opencode.json"), project.join("opencode.jsonc")],
        project: vec![ProjectRoot { path: root.clone(), present: false }],
        claude: vec![project.join(".claude")],
        agents: vec![project.join(".agents")],
    };
    (project, root, sources)
}

#[test]
fn watch_plan_groups_missing_candidates_and_keeps_parent_watches() {
    let (project, root, sources) = watch_sources();
    let missing = plan(&sources);
    assert_eq!(
        serde_json::to_value(&missing).unwrap(),
        json!([{ "path": project, "type": "entries", "names": [".agents", ".claude", ".opencode", "opencode.json", "opencode.jsonc"] }])
    );
    let present = plan(&Sources { project: vec![ProjectRoot { path: root.clone(), present: true }], ..sources.clone() });
    let mut expected = vec![json!({ "path": root, "type": "directory", "ignore": ["node_modules", ".git", "**/{node_modules,.git}/**"] })];
    expected.push(serde_json::to_value(&missing[0]).unwrap());
    assert_eq!(serde_json::to_value(&present).unwrap(), json!(expected));
    assert_eq!(present[0].key(), r#"{"path":"/watch-plan-project/.opencode","type":"directory","ignore":["node_modules",".git","**/{node_modules,.git}/**"]}"#);
}

#[test]
fn watch_plan_adds_explicit_files_only_when_not_covered() {
    let (_, root, sources) = watch_sources();
    assert_eq!(plan(&Sources { explicit: Some(sources.direct[0].clone()), ..sources.clone() }), plan(&sources));
    let present = Sources { project: vec![ProjectRoot { path: root.clone(), present: true }], ..sources.clone() };
    assert_eq!(plan(&Sources { explicit: Some(root.join("custom.json")), ..present.clone() }), plan(&present));
    let external = PathBuf::from("/watch-plan-external");
    let with = plan(&Sources { explicit: Some(external.join("custom.json")), ..sources });
    assert!(with.contains(&WatchTarget::Entries {
        path: external,
        kind: oc_config::discovery::EntriesKind::Entries,
        names: vec!["custom.json".into()],
    }));
}

// --- Markdown agents -------------------------------------------------------------------------------

#[test]
fn loads_legacy_and_native_file_based_agents() {
    let tmp = Tmp::new();
    tmp.write(
        "agents/reviewer.md",
        "---\nmodel: openrouter/openai/gpt-5\ndescription: Markdown description\ntemperature: 0.5\ntools:\n  write: false\n---\nReview carefully.",
    );
    tmp.write("agents/team/helper.md", "Help the team.");
    tmp.write(
        "agents/native.md",
        "---\nvariant: high\nrequest:\n  headers:\n    x-agent: native\n  body:\n    effort: high\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny\n---\nUse native v2 fields.",
    );
    tmp.write("agents/disabled.md", "---\ndisabled: true\n---\nDisabled");
    tmp.write("agents/empty.md", "");
    tmp.write("modes/plan.md", "Make a plan.");
    let json_doc = Entry::Document {
        path: None,
        info: Info::from_value(&json!({ "agents": { "reviewer": { "description": "JSON description" } } })).unwrap(),
    };
    let entries = vec![json_doc, Entry::Directory { path: tmp.path().to_path_buf() }];
    let docs = sources::agent_documents(&entries);
    let mut existing = IndexMap::new();
    existing.insert("build".to_string(), fold::AgentState::default());
    let folded = fold::agents(&docs, Path::new("/home/u"), existing);
    let reviewer = &folded.agents["reviewer"];
    let model = reviewer.model.as_ref().unwrap();
    assert_eq!((model.provider_id.as_str(), model.model.as_str()), ("openrouter", "openai/gpt-5"));
    assert_eq!(reviewer.system.as_deref(), Some("Review carefully."));
    assert_eq!(reviewer.description.as_deref(), Some("Markdown description"));
    assert_eq!(reviewer.body.get("temperature"), Some(&json!(0.5)));
    assert_eq!(serde_json::to_value(&reviewer.permissions).unwrap(), json!([{ "action": "edit", "resource": "*", "effect": "deny" }]));
    assert_eq!(folded.agents["team/helper"].system.as_deref(), Some("Help the team."));
    let native = &folded.agents["native"];
    assert_eq!(native.system.as_deref(), Some("Use native v2 fields."));
    assert_eq!(native.headers.get("x-agent").map(String::as_str), Some("native"));
    assert_eq!(native.body.get("effort"), Some(&json!("high")));
    assert!(native.model.is_none());
    assert!(!folded.agents.contains_key("disabled"));
    assert!(!folded.agents.contains_key("empty"));
    assert_eq!(folded.agents["plan"].mode, Some(oc_config::schema::AgentMode::Primary));
    assert!(folded.agents.contains_key("build"));
}

#[test]
fn markdown_agent_model_variant_forms() {
    let tmp = Tmp::new();
    let cases = [
        ("model: example/chat\nvariant: high", Some("example/chat#high")),
        ("model: example/chat", Some("example/chat")),
        ("model: example/chat#high", Some("example/chat#high")),
        ("model:\n  providerID: example\n  model: chat\n  variant: high", Some("example/chat#high")),
        ("variant: high", None),
        ("model: example/chat#high\nvariant: low", Some("example/chat#high")),
    ];
    for (i, (frontmatter, expected)) in cases.into_iter().enumerate() {
        let file = tmp.write(&format!("agents/a{i}.md"), &format!("---\n{frontmatter}\n---\nBody"));
        let entry = sources::decode_agent_file(tmp.path(), &file, false, &std::fs::read_to_string(&file).unwrap())
            .unwrap_or_else(|| panic!("case {i}"));
        let agent = &entry.info().unwrap().agents.as_ref().unwrap()[&format!("a{i}")];
        let rendered = agent.model.as_ref().map(|m| match &m.variant {
            Some(v) => format!("{}/{}#{}", m.provider_id, m.model, v),
            None => format!("{}/{}", m.provider_id, m.model),
        });
        assert_eq!(rendered.as_deref(), expected, "case {i}");
    }
}

#[test]
fn named_color_tokens_are_ignored_natively_but_migrated_for_v1() {
    // An unsupported color, such as a V1 theme name, drops only the color (6a5f15ad5).
    let info = Info::from_value(&json!({ "agents": { "r": { "description": "Reviews", "color": "warning" } } })).unwrap();
    let agent = &info.agents.as_ref().unwrap()["r"];
    assert_eq!((agent.description.as_deref(), agent.color.as_deref()), (Some("Reviews"), None));
    let info = Info::from_value(&json!({ "agents": { "r": { "color": 42 } } })).unwrap();
    assert_eq!(info.agents.as_ref().unwrap()["r"].color, None);
    let tmp = Tmp::new();
    let file = tmp.write("agent/r.md", "---\ncolor: warning\nhidden: true\n---\nx");
    // `color` + `hidden` are native fields, so the native decode keeps the agent without the named token.
    let native = sources::decode_agent_file(tmp.path(), &file, false, "---\ncolor: warning\n---\nx").unwrap();
    assert_eq!(native.info().unwrap().agents.as_ref().unwrap()["r"].color, None);
    let legacy = sources::decode_agent_file(tmp.path(), &file, false, "---\ncolor: warning\ntemperature: 1\n---\nx").unwrap();
    assert_eq!(legacy.info().unwrap().agents.as_ref().unwrap()["r"].color.as_deref(), Some("#aaaaaa"));
}

#[test]
fn agent_permissions_expand_home_for_path_actions() {
    let entries = vec![Entry::Document {
        path: None,
        info: Info::from_value(&json!({
            "permissions": [
                { "action": "read", "resource": "~/secret/**", "effect": "deny" },
                { "action": "shell", "resource": "$HOME/x", "effect": "deny" },
                { "action": "external_directory", "resource": "$HOME", "effect": "allow" },
            ],
            "agents": { "custom": { "permissions": [{ "action": "edit", "resource": "~", "effect": "ask" }] }, "gone": { "disabled": true } },
        }))
        .unwrap(),
    }];
    let mut existing = IndexMap::new();
    existing.insert("gone".to_string(), fold::AgentState::default());
    let folded = fold::agents(&entries, Path::new("/home/u"), existing);
    assert!(!folded.agents.contains_key("gone"));
    let rules: Vec<(String, String)> =
        folded.agents["custom"].permissions.iter().map(|r| (r.action.clone(), r.resource.clone())).collect();
    assert_eq!(rules, vec![
        ("read".into(), "/home/u/secret/**".into()),
        ("shell".into(), "$HOME/x".into()),
        ("external_directory".into(), "/home/u".into()),
        ("edit".into(), "/home/u".into()),
    ]);
}

// --- Markdown commands -------------------------------------------------------------------------------

#[test]
fn commands_fold_with_markdown_directory_slot() {
    let tmp = Tmp::new();
    tmp.write("commands/review.md", "---\ndescription: Review\nsubtask: true\n---\n\nReview $ARGUMENTS\n");
    tmp.write("command/nested/deploy.md", "Deploy $1");
    tmp.write("commands/bad.md", "---\ntemplate: x\nagent: 1\n---\nBad");
    let entries = vec![
        Entry::Document { path: None, info: Info::from_value(&json!({ "commands": { "review": { "template": "old" }, "keep": { "template": "k" } } })).unwrap() },
        Entry::Directory { path: tmp.path().to_path_buf() },
        Entry::Document { path: None, info: Info::from_value(&json!({ "commands": { "keep": { "template": "new" } } })).unwrap() },
    ];
    let commands = sources::commands(&entries);
    assert_eq!(commands.keys().cloned().collect::<Vec<_>>(), vec!["review", "keep", "nested/deploy"]);
    assert_eq!(commands["review"].template, "Review $ARGUMENTS");
    assert_eq!(commands["review"].subtask, Some(true));
    assert_eq!(commands["keep"].template, "new");
    assert!(sources::is_command_source(&entries, &tmp.path().join("commands/review.md")));
    assert!(!sources::is_agent_source(&entries, &tmp.path().join("commands/review.md")));
}

// --- Skills -----------------------------------------------------------------------------------------

#[test]
fn skill_file_parse_ids_and_flags() {
    let dir = Path::new("/repo/skills");
    let parse = |fm: &str| {
        sources::parse_skill(dir, Path::new("/repo/skills/deploy/SKILL.md"), &format!("---\n{fm}\n---\n# deploy")).unwrap()
    };
    assert_eq!(parse("disable-model-invocation: true").autoinvoke, Some(false));
    assert_eq!(parse("disable-model-invocation: yes").autoinvoke, Some(false));
    assert_eq!(parse("disable-model-invocation: false").autoinvoke, None);
    assert_eq!(parse("disable-model-invocation: maybe").autoinvoke, None);
    assert_eq!(parse("disable-model-invocation: true\nmetadata:\n  opencode/autoinvoke: true").autoinvoke, Some(true));

    let manual = sources::parse_skill(
        dir,
        Path::new("/repo/skills/manual/SKILL.md"),
        "---\nname: Manual\ndescription: Manual only\nmetadata:\n  opencode/autoinvoke: false\n---\n# manual",
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&manual).unwrap(),
        json!({ "id": "manual", "name": "Manual", "description": "Manual only", "autoinvoke": false, "path": "/repo/skills/manual/SKILL.md", "content": "# manual" })
    );
    assert_eq!(sources::parse_skill(dir, Path::new("/repo/skills/foo.md"), "# foo").unwrap().id, "foo");
    let nested = sources::parse_skill(Path::new("/repo/skills/manual"), Path::new("/repo/skills/manual/SKILL.md"), "# manual").unwrap();
    assert_eq!((nested.id.as_str(), nested.name.as_str()), ("manual", "manual"));
    assert_eq!(
        sources::parse_skill(dir, Path::new("/repo/skills/broken.md"), "---\ndescription: foo: bar\nmetadata: [\n---\n# broken"),
        Err(sources::SkillSkip::Markdown)
    );
}

#[test]
fn skill_sources_and_scanning() {
    let tmp = Tmp::new();
    let opencode = tmp.mkdir("opencode");
    tmp.write("opencode/skills/deploy/SKILL.md", "---\ndescription: Deploy\n---\nsteps");
    tmp.write("opencode/skills/quick.md", "quick");
    tmp.write("opencode/skills/deep/nested/notes.md", "ignored");
    let entries = vec![
        Entry::Directory { path: opencode.clone() },
        Entry::Document { path: None, info: Info::from_value(&json!({ "skills": ["~/shared", "./relative", "https://example.com/skills", "./relative"] })).unwrap() },
    ];
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    let sources_ = sources::skill_sources(&entries, &home, &project);
    assert_eq!(serde_json::to_value(&sources_).unwrap(), json!([
        { "type": "directory", "path": opencode.join("skill") },
        { "type": "directory", "path": opencode.join("skills") },
        { "type": "directory", "path": home.join("shared") },
        { "type": "directory", "path": project.join("relative") },
        { "type": "url", "url": "https://example.com/skills" },
    ]));
    let skills = sources::scan_skills(&opencode.join("skills"));
    assert_eq!(skills.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["deploy", "quick"]);
    tmp.write("claude/skills/review/SKILL.md", "review");
    let compat = sources::compatibility_skills(&[tmp.path().join("claude")], &[tmp.path().join("missing")]);
    assert_eq!(compat.len(), 1);
}

// --- Plugins ----------------------------------------------------------------------------------------

#[test]
fn plugin_directory_discovery_and_operations() {
    let tmp = Tmp::new();
    let root = tmp.mkdir("root");
    tmp.write("root/plugin/b.ts", "");
    tmp.write("root/plugin/readme.md", "");
    tmp.mkdir("root/plugins/a-dir");
    tmp.write("root/plugins/c.js", "");
    let file_plugin = tmp.write("files/plugin.ts", "");
    let doc_path = tmp.write("project/opencode.json", "{}");
    let entries = vec![
        Entry::Directory { path: root.clone() },
        Entry::Document {
            path: Some(doc_path),
            info: Info::from_value(&json!({ "plugins": ["pkg", "-other", "./local", { "package": "x", "options": { "a": 1 } }, file_plugin.to_string_lossy()] })).unwrap(),
        },
    ];
    let ops = sources::plugin_operations(&entries, tmp.path()).unwrap();
    assert_eq!(serde_json::to_value(&ops).unwrap(), json!([
        { "type": "add", "target": root.join("plugin/b.ts"), "options": {} },
        { "type": "add", "target": root.join("plugins/a-dir"), "options": {} },
        { "type": "add", "target": root.join("plugins/c.js"), "options": {} },
        { "type": "add", "target": "pkg", "options": {} },
        { "type": "remove", "target": "other" },
        { "type": "add", "target": tmp.path().join("project/local"), "options": {} },
        { "type": "add", "target": "x", "options": { "a": 1 } },
    ]));
    let bad = vec![Entry::Document { path: None, info: Info::from_value(&json!({ "plugins": ["-"] })).unwrap() }];
    assert_eq!(sources::plugin_operations(&bad, tmp.path()).unwrap_err(), "Plugin remove operation requires a target");
}

// --- Folds ------------------------------------------------------------------------------------------

fn doc(value: serde_json::Value) -> Entry {
    Entry::Document { path: None, info: Info::from_value(&value).unwrap() }
}

#[test]
fn per_key_folds() {
    let entries = vec![
        doc(json!({
            "formatter": { "prettier": { "command": ["a"], "environment": { "A": "1" } } },
            "mcp": { "timeout": { "catalog": 1 }, "servers": { "s": { "type": "local", "command": ["x"], "timeout": { "execution": 5 } } } },
            "compaction": { "auto": false, "keep": { "tokens": 10 } },
            "media": { "image": { "max_width": 100 } },
            "watcher": { "ignore": ["a"] },
            "tool_output": { "max_lines": 5, "max_bytes": 6 },
            "skills": ["x", "y"],
            "references": { "docs": "./docs", "repo": "github.com/a/b", "bad name": "./x" },
        })),
        doc(json!({
            "formatter": { "prettier": { "extensions": [".ts"], "environment": { "B": "2" } }, "black": { "disabled": true } },
            "mcp": { "timeout": { "startup": 2 }, "servers": { "s": { "type": "remote", "url": "https://x" } } },
            "compaction": { "buffer": 3 },
            "media": { "image": { "auto_resize": false } },
            "watcher": { "ignore": ["b"] },
            "tool_output": { "max_lines": 7 },
            "skills": ["y", "z"],
            "references": { "docs": { "path": "~/d", "hidden": true } },
            "warming": { "interval": "1 minutes" },
        })),
    ];
    let Some(fold::FormatterConfig::Entries(f)) = fold::formatter(&entries) else { panic!() };
    assert_eq!(serde_json::to_value(&f).unwrap(), json!({
        "prettier": { "command": ["a"], "environment": { "A": "1", "B": "2" }, "extensions": [".ts"] },
        "black": { "disabled": true, "environment": {} },
    }));
    let mut reset = entries.clone();
    reset.push(doc(json!({ "formatter": false })));
    assert_eq!(fold::formatter(&reset), Some(fold::FormatterConfig::Enabled(false)));

    let servers = fold::mcp_servers(&entries);
    assert_eq!(serde_json::to_value(&servers).unwrap(), json!({ "s": { "type": "remote", "url": "https://x", "timeout": { "startup": 2, "catalog": 1 } } }));
    assert_eq!(fold::compaction(&entries), fold::CompactionSettings { auto: Some(false), buffer: Some(3), keep: Some(10) });
    let image = fold::media_image(&entries);
    assert_eq!((image.auto_resize, image.max_width, image.max_height), (false, 100, 2000));
    assert_eq!(fold::watcher_ignore(&entries), vec!["a", "b"]);
    assert_eq!(fold::tool_output(&entries).unwrap().max_bytes, None);
    assert_eq!(fold::skills(&entries), vec!["x", "y", "z"]);
    let refs = fold::references(&entries, Path::new("/home/u"), Path::new("/loc"));
    assert_eq!(serde_json::to_value(&refs).unwrap(), json!({
        "docs": { "type": "local", "path": "/home/u/d", "hidden": true },
        "repo": { "type": "git", "repository": "github.com/a/b" },
    }));
    let w = fold::warming(&entries).unwrap();
    assert_eq!(w.interval.to_millis(), 60_000.0);
    assert_eq!(w.duration.to_millis(), 1_800_000.0);
    assert_eq!(w.prompt, fold::WARMING_PROMPT);
    assert!(fold::warming(&[doc(json!({ "warming": { "interval": "0 millis" } }))]).is_none());
    assert!(fold::warming(&[doc(json!({ "warming": false }))]).is_none());
    assert_eq!(fold::subagent_depth(&entries), 1);
}

#[test]
fn provider_fold_merges_settings_and_headers() {
    let entries = vec![
        doc(json!({ "providers": { "p": {
            "name": "A", "settings": { "nested": { "a": 1, "b": 1 }, "timeout": 5 }, "headers": { "X-Key": "1", "Keep": "k" },
            "models": { "m": { "name": "M", "variants": [{ "id": "high", "settings": { "e": 1 } }], "limit": { "context": 10 } } },
        }}})),
        doc(json!({ "providers": { "p": {
            "settings": { "nested": { "b": 2 } }, "headers": { "x-key": "2" },
            "models": { "m": { "cost": { "input": 1, "output": 2 }, "variants": [{ "id": "high", "settings": { "f": 2 } }, { "id": "low" }], "limit": { "output": 5 } } },
        }}})),
    ];
    let providers = fold::providers(&entries);
    assert_eq!(serde_json::to_value(&providers["p"]).unwrap(), json!({
        "name": "A",
        "settings": { "timeout": 5, "nested": { "a": 1, "b": 2 } },
        "headers": { "x-key": "2", "Keep": "k" },
        "models": { "m": {
            "name": "M",
            "variants": [{ "id": "high", "settings": { "e": 1, "f": 2 } }, { "id": "low" }],
            "cost": [{ "input": 1, "output": 2, "cache": { "read": 0, "write": 0 } }],
            "limit": { "context": 10, "output": 5 },
        }},
    }));
}

#[test]
fn update_shell_preserves_comments() {
    let tmp = Tmp::new();
    let global = tmp.mkdir("global");
    let file = update_shell(&global, Some("/bin/zsh")).unwrap();
    assert_eq!(file, global.join("opencode.jsonc"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\n  \"shell\": \"/bin/zsh\"\n}\n");
    std::fs::write(global.join("opencode.json"), "{\n  // keep me\n  \"shell\": \"a\",\n  \"username\": \"u\"\n}").unwrap();
    let file = update_shell(&global, None).unwrap();
    assert_eq!(file, global.join("opencode.jsonc"));
    std::fs::remove_file(global.join("opencode.jsonc")).unwrap();
    let file = update_shell(&global, Some("b")).unwrap();
    assert_eq!(file, global.join("opencode.json"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\n  // keep me\n  \"shell\": \"b\",\n  \"username\": \"u\"\n}\n");
    // jsonc-parser removes everything between `{` and the next property, comments included.
    update_shell(&global, None).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\n  \"username\": \"u\"\n}\n");
}

#[test]
fn entries_serialize_like_the_http_api() {
    let entries = vec![
        Entry::Document { path: Some("/a/opencode.json".into()), info: Info::from_value(&json!({ "model": "a/b#c", "shell": "x" })).unwrap() },
        Entry::Directory { path: "/a/.opencode".into() },
    ];
    assert_eq!(
        serde_json::to_string(&entries).unwrap(),
        r#"[{"type":"document","path":"/a/opencode.json","info":{"shell":"x","model":{"providerID":"a","model":"b","variant":"c"}}},{"type":"directory","path":"/a/.opencode"}]"#
    );
    let back: Vec<Entry> = serde_json::from_str(&serde_json::to_string(&entries).unwrap()).unwrap();
    assert_eq!(back, entries);
}
