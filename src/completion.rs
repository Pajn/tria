//! What the composer offers to finish a `/` command or a `$` skill with: the provider's
//! own lists, filtered and ranked by what has been typed.

use crate::composer::{Trigger, TriggerKind};
use crate::model::{Skill, SlashCommand};

/// How many rows the list shows at once.
pub const VISIBLE: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What replaces the typed word, trailing space and all.
    pub insert: String,
    /// What the row says the item is.
    pub label: String,
    pub detail: String,
}

/// Everything the trigger could become, best first. A skill is offered under `/` as
/// well as `$`, since `/` is where somebody looking for anything to run starts, but
/// it is always written as `$name`: that is what the server reads as a skill. A
/// command that is also a skill is shown once, as the skill.
pub fn items(trigger: &Trigger, commands: &[SlashCommand], skills: &[Skill]) -> Vec<Item> {
    let skills: Vec<&Skill> = skills.iter().filter(|skill| skill.offered()).collect();
    let mut ranked: Vec<(u8, Item)> = Vec::new();
    if trigger.kind
        == (TriggerKind::Command {
            at_prompt_start: true,
        })
    {
        for command in commands {
            if command.user_invocable == Some(false)
                || skills
                    .iter()
                    .any(|skill| skill.name.eq_ignore_ascii_case(&command.name))
            {
                continue;
            }
            let description = command.description.clone().unwrap_or_default();
            let Some(rank) = rank(&command.name, &description, &trigger.query) else {
                continue;
            };
            let label = match &command.input {
                Some(input) => format!("/{} {}", command.name, input.hint),
                None => format!("/{}", command.name),
            };
            ranked.push((
                rank,
                Item {
                    insert: format!("/{} ", command.name),
                    label,
                    detail: description,
                },
            ));
        }
    }
    for skill in skills {
        let description = skill
            .short_description
            .clone()
            .or_else(|| skill.description.clone())
            .unwrap_or_default();
        let Some(rank) = rank(&skill.name, &description, &trigger.query) else {
            continue;
        };
        ranked.push((
            rank,
            Item {
                insert: format!("${} ", skill.name),
                label: format!("${}", skill.name),
                detail: description,
            },
        ));
    }
    ranked.sort_by(|(a, left), (b, right)| a.cmp(b).then_with(|| left.label.cmp(&right.label)));
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| seen.insert(item.insert.clone()))
        .collect()
}

/// How well a name answers what was typed, lower being better, or nothing when it
/// does not: the name starting with it, then a part of the name starting with it, then
/// the name holding it anywhere, then the description holding it.
fn rank(name: &str, description: &str, query: &str) -> Option<u8> {
    let query = query.to_lowercase();
    let name = name.to_lowercase();
    if name.starts_with(&query) {
        return Some(0);
    }
    if name
        .split(['-', '_', ':', '/'])
        .skip(1)
        .any(|part| part.starts_with(&query))
    {
        return Some(1);
    }
    if name.contains(&query) {
        return Some(2);
    }
    description.to_lowercase().contains(&query).then_some(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn commands() -> Vec<SlashCommand> {
        serde_json::from_value(json!([
            {"name": "compact", "description": "Summarise the conversation"},
            {"name": "review", "description": "Review a pull request", "input": {"hint": "<pr>"}},
            {"name": "only-the-agent", "userInvocable": false},
            {"name": "deploy", "description": "the skill's own command"},
        ]))
        .unwrap()
    }

    fn skills() -> Vec<Skill> {
        serde_json::from_value(json!([
            {"name": "deploy", "enabled": true, "shortDescription": "Ship it"},
            {"name": "code-review", "enabled": true},
            {"name": "off", "enabled": false},
        ]))
        .unwrap()
    }

    fn trigger(kind: TriggerKind, query: &str) -> Trigger {
        Trigger {
            kind,
            query: query.into(),
            start: 0,
        }
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    #[test]
    fn a_slash_offers_commands_and_skills_but_not_what_only_the_agent_runs() {
        let start = TriggerKind::Command {
            at_prompt_start: true,
        };
        let all = items(&trigger(start, ""), &commands(), &skills());
        assert_eq!(
            labels(&all),
            ["$code-review", "$deploy", "/compact", "/review <pr>"]
        );
        let deploy = all.iter().find(|item| item.label == "$deploy").unwrap();
        assert_eq!(deploy.insert, "$deploy ");
        assert_eq!(deploy.detail, "Ship it");
    }

    #[test]
    fn the_start_of_the_name_wins_over_a_part_of_it() {
        let start = TriggerKind::Command {
            at_prompt_start: true,
        };
        let found = items(&trigger(start, "rev"), &commands(), &skills());
        assert_eq!(labels(&found), ["/review <pr>", "$code-review"]);
    }

    #[test]
    fn a_command_is_only_a_command_at_the_start_of_the_message() {
        let later = TriggerKind::Command {
            at_prompt_start: false,
        };
        assert_eq!(
            labels(&items(&trigger(later, ""), &commands(), &skills())),
            ["$code-review", "$deploy"]
        );
        assert_eq!(
            labels(&items(
                &trigger(TriggerKind::Skill, "dep"),
                &commands(),
                &skills()
            )),
            ["$deploy"]
        );
    }
}
