//! Finished-turn wall time. Historical checkpoints have an end but no start;
//! their estimate uses the prompt preceding the turn's first recorded work.
use std::collections::BTreeMap;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::state::ThreadState;

pub struct Finished {
    pub id: String,
    pub completed_at: String,
    pub label: String,
}

pub fn finished(thread: &ThreadState) -> Vec<Finished> {
    let detail = &thread.detail;
    let mut ends: BTreeMap<&str, &str> = detail
        .checkpoints
        .iter()
        .map(|c| (c.turn_id.as_str(), c.completed_at.as_str()))
        .collect();
    for turn in thread
        .turn_timings
        .values()
        .chain(detail.shell.latest_turn.iter())
    {
        if let Some(end) = turn.completed_at.as_deref()
            && matches!(turn.state.as_str(), "completed" | "interrupted" | "error")
        {
            ends.insert(&turn.turn_id, end);
        }
    }
    ends.into_iter()
        .filter_map(|(id, end)| {
            if detail
                .shell
                .latest_turn
                .as_ref()
                .is_some_and(|t| t.turn_id == id && t.state == "running")
            {
                return None;
            }
            let exact = detail
                .shell
                .latest_turn
                .as_ref()
                .filter(|t| t.turn_id == id)
                .or_else(|| thread.turn_timings.get(id));
            let (start, estimated) = if let Some(turn) = exact {
                match turn.started_at.as_deref() {
                    Some(start) => (parse(start)?, false),
                    None => (parse(&turn.requested_at)?, true),
                }
            } else {
                // Only synchronous work establishes the beginning: a background task can
                // speak long after its owning turn has completed.
                let first = detail
                    .messages
                    .iter()
                    .filter(|m| m.turn_id.as_deref() == Some(id))
                    .filter_map(|m| parse(&m.created_at))
                    .chain(
                        detail
                            .activities
                            .iter()
                            .filter(|a| {
                                a.turn_id.as_deref() == Some(id) && a.kind.starts_with("tool.")
                            })
                            .filter_map(|a| parse(&a.created_at)),
                    )
                    .min()?;
                let previous_end = detail
                    .checkpoints
                    .iter()
                    .filter(|c| c.turn_id != id)
                    .filter_map(|c| parse(&c.completed_at))
                    .filter(|at| *at < first)
                    .max();
                let prompt = detail
                    .messages
                    .iter()
                    .filter(|m| m.role == "user")
                    .filter_map(|m| parse(&m.created_at))
                    .filter(|at| *at <= first && previous_end.is_none_or(|end| *at >= end))
                    .max()?;
                (prompt, true)
            };
            let elapsed = parse(end)? - start;
            if elapsed.is_negative() {
                return None;
            }
            let seconds = elapsed.whole_seconds();
            let suffix = match exact.map(|turn| turn.state.as_str()) {
                Some("interrupted") => " · interrupted",
                Some("error") => " · failed",
                _ => "",
            };
            Some(Finished {
                id: id.to_string(),
                completed_at: end.to_string(),
                label: format!(
                    "Worked for {}{}{suffix}",
                    if estimated { "~" } else { "" },
                    duration(seconds)
                ),
            })
        })
        .collect()
}

fn parse(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

fn duration(seconds: i64) -> String {
    if seconds < 1 {
        "<1s".to_string()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread() -> ThreadState {
        ThreadState::from_snapshot(serde_json::from_value(json!({
            "snapshotSequence":1,
            "thread":{
                "id":"thread", "projectId":"p", "title":"timing",
                "modelSelection":{"instanceId":"provider","model":"model"},
                "latestTurn":{"turnId":"one", "state":"completed",
                    "requestedAt":"2026-09-22T12:00:00Z", "startedAt":"2026-09-22T12:00:05Z",
                    "completedAt":"2026-09-22T12:02:08Z"},
                "messages":[
                    {"id":"prompt","role":"user","text":"go","createdAt":"2026-09-22T12:00:00Z"},
                    {"id":"progress","role":"assistant","text":"working","turnId":"one","createdAt":"2026-09-22T12:00:20Z"},
                    {"id":"steering","role":"user","text":"also this","createdAt":"2026-09-22T12:01:00Z"},
                    {"id":"answer","role":"assistant","text":"done","turnId":"one","createdAt":"2026-09-22T12:02:07Z"}
                ],
                "checkpoints":[{"turnId":"one","completedAt":"2026-09-22T12:02:08Z"}]
            }
        })).unwrap())
    }

    #[test]
    fn exact_elapsed_time_survives_the_next_turn_and_is_rendered_after_the_answer() {
        let mut thread = thread();
        assert_eq!(finished(&thread)[0].label, "Worked for 2m 3s");
        let mut shell = thread.detail.shell.clone();
        shell.latest_turn = Some(
            serde_json::from_value(json!({
                "turnId":"two", "state":"running", "requestedAt":"2026-09-22T12:03:00Z",
                "startedAt":"2026-09-22T12:03:01Z"
            }))
            .unwrap(),
        );
        thread.sync_shell(&shell);
        assert_eq!(finished(&thread).len(), 1);
        assert_eq!(finished(&thread)[0].label, "Worked for 2m 3s");
        let blocks = crate::timeline::build(&thread, &Default::default(), 0, 100, 30);
        let answer = blocks
            .iter()
            .position(|b| b.key == crate::timeline::BlockKey::Message("answer".into()))
            .unwrap();
        assert_eq!(
            blocks[answer + 1].key,
            crate::timeline::BlockKey::TurnEnd("one".into())
        );
        assert!(
            blocks[answer + 1]
                .text
                .to_string()
                .contains("◷ Worked for 2m 3s")
        );
        assert!(blocks[answer + 1].rows.is_empty());
    }

    #[test]
    fn history_uses_original_prompt_not_mid_turn_steering_and_marks_estimates() {
        let mut thread = thread();
        thread.detail.shell.latest_turn = None;
        thread.turn_timings.clear();
        assert_eq!(finished(&thread)[0].label, "Worked for ~2m 8s");
        // A page missing the beginning must not invent a start from later steering.
        thread.detail.messages.retain(|m| m.id != "prompt");
        assert!(finished(&thread).is_empty());
    }

    #[test]
    fn live_turns_and_bad_timestamps_do_not_get_finished_rows() {
        let mut thread = thread();
        thread.detail.shell.latest_turn.as_mut().unwrap().state = "running".into();
        assert!(finished(&thread).is_empty());
        let turn = thread.detail.shell.latest_turn.as_mut().unwrap();
        turn.state = "completed".into();
        turn.started_at = Some("not a date".into());
        assert!(finished(&thread).is_empty());
        thread.detail.shell.latest_turn.as_mut().unwrap().started_at =
            Some("2026-09-22T14:00:00Z".into());
        assert!(finished(&thread).is_empty());
    }

    #[test]
    fn interrupted_and_failed_turns_are_named_and_units_are_readable() {
        let mut thread = thread();
        thread.detail.shell.latest_turn.as_mut().unwrap().state = "interrupted".into();
        assert_eq!(finished(&thread)[0].label, "Worked for 2m 3s · interrupted");
        thread.detail.shell.latest_turn.as_mut().unwrap().state = "error".into();
        assert_eq!(finished(&thread)[0].label, "Worked for 2m 3s · failed");
        assert_eq!(duration(0), "<1s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(60), "1m 0s");
        assert_eq!(duration(3661), "1h 1m 1s");
    }

    #[test]
    fn completion_events_are_retained_and_deduplicated() {
        let mut thread = thread();
        thread.detail.checkpoints.clear();
        for sequence in [2, 3] {
            thread.apply_event(
                serde_json::from_value(json!({
                    "sequence":sequence, "type":"thread.turn-diff-completed",
                    "payload":{"turnId":"one","completedAt":"2026-09-22T12:02:08Z"}
                }))
                .unwrap(),
            );
        }
        assert_eq!(thread.detail.checkpoints.len(), 1);
    }
}
