//! Statusline admission policy (`src/mini/footer.width.ts`).

use crate::util::locale::{self, width as sw};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupId {
    Spinner,
    Status,
    Escape,
    Queued,
    Subagents,
    Background,
    Agent,
    Model,
    Context,
    Cost,
    Provider,
    Menu,
}

const ORDER: [GroupId; 12] = [
    GroupId::Spinner,
    GroupId::Status,
    GroupId::Escape,
    GroupId::Queued,
    GroupId::Subagents,
    GroupId::Background,
    GroupId::Agent,
    GroupId::Model,
    GroupId::Context,
    GroupId::Provider,
    GroupId::Cost,
    GroupId::Menu,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text,
    Muted,
    Agent,
    Status,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub id: GroupId,
    pub parts: Vec<(String, Tone)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub key: String,
    pub label: String,
    pub expanded: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct StatusInput {
    pub width: usize,
    pub mono: bool,
    pub status: Option<(String, Option<String>)>,
    pub escape: Option<Action>,
    pub work: Vec<(GroupId, Action)>,
    pub model: Option<(String, Option<String>)>,
    pub agent: Option<String>,
    pub context: Option<(String, String)>,
    pub cost: Option<String>,
    pub provider: Option<String>,
    pub menu: Option<Action>,
    pub spinner: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub groups: Vec<Group>,
    pub text: String,
}

fn group(id: GroupId, text: &str, tone: Tone) -> Group {
    Group { id, parts: vec![(text.to_string(), tone)] }
}

fn action(id: GroupId, value: &Action, expanded: bool) -> Group {
    let label = if expanded { value.expanded.clone().unwrap_or(value.label.clone()) } else { value.label.clone() };
    Group { id, parts: vec![(value.key.clone(), Tone::Text), (format!(" {label}"), Tone::Muted)] }
}

/// `footerStatuslinePolicy(input)`.
pub fn footer_statusline_policy(input: &StatusInput) -> Layout {
    let identity = |cells: Option<usize>| {
        let (name, variant) = input.model.clone().unwrap();
        let ellipsis = if input.mono { "..." } else { "\u{2026}" };
        let text = match cells {
            Some(c) if sw(&name) > c + sw(ellipsis) => format!("{}{ellipsis}", locale::take_width(&name, c).trim_end()),
            _ => name,
        };
        group(GroupId::Model, &format!("{text}{}", variant.map(|v| format!(" [{v}]")).unwrap_or_default()), Tone::Text)
    };
    let mut selected: Vec<Group> = Vec::new();
    let set = |selected: &mut Vec<Group>, g: Group| match selected.iter_mut().find(|x| x.id == g.id) {
        Some(slot) => *slot = g,
        None => selected.push(g),
    };
    if let Some(s) = &input.spinner {
        set(&mut selected, group(GroupId::Spinner, s, Tone::Text));
    }
    if let Some((text, _)) = &input.status
        && !text.is_empty()
    {
        set(&mut selected, group(GroupId::Status, text, Tone::Status));
    }
    if let Some(e) = &input.escape {
        set(&mut selected, action(GroupId::Escape, e, false));
    }
    let place = |selected: &Vec<Group>| -> Layout {
        let groups: Vec<Group> = ORDER.iter().filter_map(|id| selected.iter().find(|g| g.id == *id).cloned()).collect();
        let separator = if input.mono { " - " } else { " \u{b7} " };
        let mut text = String::new();
        for (index, g) in groups.iter().enumerate() {
            if index > 0 {
                text.push_str(if groups[index - 1].id == GroupId::Spinner { " " } else { separator });
            }
            for (t, _) in &g.parts {
                text.push_str(t);
            }
        }
        Layout { groups, text }
    };
    let mut layout = place(&selected);
    // Required controls may wrap. Optional information never crowds that fallback.
    if sw(&layout.text) > input.width || layout.text.contains('\n') {
        return layout;
    }
    // Each stage retains earlier information. Stop at the first non-fitting stage.
    let mut stages: Vec<Group> = input.work.iter().map(|(id, a)| action(*id, a, false)).collect();
    if input.model.is_some() {
        stages.push(identity(Some(8)));
    }
    if let Some(a) = &input.agent {
        stages.push(group(GroupId::Agent, a, Tone::Agent));
    }
    if let Some((compact, _)) = &input.context {
        stages.push(group(GroupId::Context, compact, Tone::Muted));
    }
    if input.model.is_some() {
        stages.push(identity(Some(24)));
    }
    if let Some((_, full)) = &input.context {
        stages.push(group(GroupId::Context, full, Tone::Muted));
    }
    if let Some(c) = &input.cost {
        stages.push(group(GroupId::Cost, c, Tone::Muted));
    }
    if let Some(p) = &input.provider {
        stages.push(group(GroupId::Provider, p, Tone::Muted));
    }
    if let Some(m) = &input.menu {
        stages.push(action(GroupId::Menu, m, false));
    }
    if input.model.is_some() {
        stages.push(identity(None));
    }
    stages.extend(input.work.iter().filter(|(_, a)| a.expanded.is_some()).map(|(id, a)| action(*id, a, true)));
    if let Some((_, Some(expanded))) = &input.status {
        stages.push(group(GroupId::Status, expanded, Tone::Status));
    }
    for stage in stages {
        set(&mut selected, stage);
        let next = place(&selected);
        if sw(&next.text) > input.width || next.text.contains('\n') {
            break;
        }
        layout = next;
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screenshot() -> StatusInput {
        StatusInput {
            model: Some(("GPT-5.6 Sol (50% Off)".into(), Some("max".into()))),
            agent: Some("Build".into()),
            context: Some(("1% ctx".into(), "14.1K (1%)".into())),
            cost: Some("$0.04".into()),
            provider: Some("Anomaly / OpenCode".into()),
            menu: Some(Action { key: "ctrl+p".into(), label: "menu".into(), expanded: None }),
            ..Default::default()
        }
    }

    fn ids(l: &Layout) -> Vec<GroupId> {
        l.groups.iter().map(|g| g.id).collect()
    }

    #[test]
    fn progressive_layouts_fit_and_preserve() {
        for mono in [false, true] {
            let shot = screenshot();
            let fixtures = vec![
                StatusInput::default(),
                shot.clone(),
                StatusInput { model: Some(("GPT-5".into(), Some("xhigh".into()))), ..shot.clone() },
                StatusInput { model: Some(("a model name with a very long and informative suffix".repeat(3), None)), ..shot.clone() },
                StatusInput { model: Some(("\u{6a21}\u{578b}\u{1f680}e\u{301}\u{5f00}\u{53d1} Model".repeat(3), Some("\u{9ad8}\u{7ea7}".into()))), ..shot.clone() },
                StatusInput { model: Some(("GPT-5".into(), Some("an-unusually-long-but-complete-variant".into()))), ..shot.clone() },
                StatusInput { status: Some(("esc stop".into(), Some("esc interrupt".into()))), spinner: Some(if mono { "*" } else { "\u{25aa}" }.into()), ..shot.clone() },
                StatusInput {
                    status: Some(("ctrl+shift+alt+i stop".into(), Some("ctrl+shift+alt+i interrupt".into()))),
                    work: vec![
                        (GroupId::Queued, Action { key: "ctrl+x q".into(), label: "12 queued".into(), expanded: None }),
                        (GroupId::Subagents, Action { key: if mono { "down" } else { "\u{2193}" }.into(), label: "2 sub".into(), expanded: Some("2 subagents".into()) }),
                        (GroupId::Background, Action { key: "ctrl+b".into(), label: "bg".into(), expanded: Some("background".into()) }),
                    ],
                    ..shot.clone()
                },
                StatusInput { status: Some(("Shell".into(), None)), escape: Some(Action { key: "esc".into(), label: "normal".into(), expanded: None }), context: shot.context.clone(), ..Default::default() },
            ];
            for fixture in fixtures {
                let mut previous: Vec<GroupId> = vec![];
                for width in 0..=280 {
                    let layout = footer_statusline_policy(&StatusInput { mono, width, ..fixture.clone() });
                    let ids = ids(&layout);
                    if let Some(sp) = &fixture.spinner {
                        assert_eq!(ids[0], GroupId::Spinner);
                        assert!(layout.text.starts_with(&format!("{sp} ")));
                        if width >= sw(&format!("{sp} {}", fixture.status.as_ref().unwrap().0)) {
                            assert!(sw(&layout.text) <= width);
                        }
                    }
                    if sw(&layout.text) > width {
                        assert!(ids.iter().all(|id| matches!(id, GroupId::Spinner | GroupId::Status | GroupId::Escape)));
                    }
                    for id in &previous {
                        assert!(ids.contains(id), "{id:?} dropped at {width}");
                    }
                    let mut uniq = ids.clone();
                    uniq.dedup();
                    assert_eq!(uniq.len(), ids.len());
                    if ids.contains(&GroupId::Menu) {
                        assert_eq!(ids.last(), Some(&GroupId::Menu));
                    }
                    if let (Some(identity), Some((_, Some(variant)))) = (layout.groups.iter().find(|g| g.id == GroupId::Model), &fixture.model) {
                        assert!(identity.parts.iter().map(|p| p.0.as_str()).collect::<String>().ends_with(&format!(" [{variant}]")));
                    }
                    assert!(!layout.text.contains('\u{fffd}'));
                    previous = ids;
                }
            }
        }
    }

    #[test]
    fn identity_admission_keeps_variant() {
        let input = StatusInput { model: Some(("GPT-5".into(), Some("max".into()))), ..Default::default() };
        assert!(footer_statusline_policy(&StatusInput { width: 10, ..input.clone() }).groups.is_empty());
        assert_eq!(footer_statusline_policy(&StatusInput { width: 11, ..input }).text, "GPT-5 [max]");
    }

    #[test]
    fn allocation_priority_separate_from_placement() {
        assert_eq!(ids(&footer_statusline_policy(&StatusInput { width: 15, ..screenshot() })), vec![GroupId::Model]);
        assert_eq!(ids(&footer_statusline_policy(&StatusInput { width: 60, ..screenshot() })), vec![GroupId::Agent, GroupId::Model, GroupId::Context, GroupId::Cost]);
        assert_eq!(
            footer_statusline_policy(&StatusInput { width: 112, ..screenshot() }).text,
            "Build \u{b7} GPT-5.6 Sol (50% Off) [max] \u{b7} 14.1K (1%) \u{b7} Anomaly / OpenCode \u{b7} $0.04 \u{b7} ctrl+p menu"
        );
    }

    #[test]
    fn no_backfill() {
        let input = StatusInput { work: vec![(GroupId::Queued, Action { key: "ctrl+shift+alt+q".into(), label: "12 queued".into(), expanded: None })], ..screenshot() };
        assert!(footer_statusline_policy(&StatusInput { width: 24, ..input.clone() }).groups.is_empty());
        assert_eq!(ids(&footer_statusline_policy(&StatusInput { width: 26, ..input })), vec![GroupId::Queued]);
    }

    #[test]
    fn context_and_cost() {
        let input = StatusInput { context: screenshot().context, cost: screenshot().cost, ..Default::default() };
        assert_eq!(footer_statusline_policy(&StatusInput { width: 6, ..input.clone() }).text, "1% ctx");
        assert_eq!(footer_statusline_policy(&StatusInput { width: 10, ..input.clone() }).text, "14.1K (1%)");
        assert_eq!(footer_statusline_policy(&StatusInput { width: 18, ..input }).text, "14.1K (1%) \u{b7} $0.04");
    }

    #[test]
    fn stop_label_keeps_identity() {
        let input = StatusInput { status: Some(("esc stop".into(), Some("esc interrupt".into()))), ..screenshot() };
        for width in [55, 56, 57, 79, 80, 81] {
            let layout = footer_statusline_policy(&StatusInput { width, ..input.clone() });
            assert!(layout.text.contains("GPT-5.6 Sol (50% Off) [max]"));
            assert!(layout.text.starts_with("esc stop"));
        }
        assert!(footer_statusline_policy(&StatusInput { width: 160, ..input }).text.starts_with("esc interrupt"));
    }

    #[test]
    fn required_controls_wrap() {
        assert_eq!(footer_statusline_policy(&StatusInput { width: 16, status: Some(("ctrl+shift+alt+x exit".into(), None)), ..screenshot() }).text, "ctrl+shift+alt+x exit");
        assert_eq!(footer_statusline_policy(&StatusInput { width: 80, status: Some(("Please confirm\nExit now?".into(), None)), ..screenshot() }).text, "Please confirm\nExit now?");
    }
}
