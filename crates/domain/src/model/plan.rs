//! The agent's work plan: a short checklist the model keeps up to date.
//!
//! Only the value and its rules live here. Where the current plan comes from
//! (the conversation), when the model is reminded of it (after the history is
//! shortened) and how it is drawn (the terminal) are use cases and
//! presentation, kept out of the domain for the same reason the summary's
//! wording is kept out of [`Conversation`](super::conversation::Conversation).
//!
//! ## Rejected, never repaired
//!
//! A plan that breaks a rule is refused as a whole with a [`PlanError`] that
//! names the item. Quietly fixing it - trimming a long item, demoting a second
//! `in_progress` - would leave the model believing it had written something it
//! had not, and the next update would be built on that belief.

use serde::{Deserialize, Serialize};

use crate::error::PlanError;
use crate::text;

/// More than this is no longer a plan the user can take in at a glance, and it
/// is pinned into the history whole after every shortening.
pub const MAX_PLAN_ITEMS: usize = 20;

/// One line on a terminal, and small enough that a full plan stays a few KB.
pub const MAX_PLAN_ITEM_BYTES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One step as the model wrote it. Unvalidated on its own: the rules are about
/// the plan as a whole (and name items by position), so they are checked by
/// [`Plan::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub content: String,
    pub status: PlanStatus,
}

impl PlanItem {
    pub fn new(content: impl Into<String>, status: PlanStatus) -> Self {
        Self {
            content: content.into(),
            status,
        }
    }
}

/// A checklist that satisfies every rule below. The only way to get one is
/// [`Plan::new`], so holding a `Plan` means holding a valid one.
///
/// * at most [`MAX_PLAN_ITEMS`] items;
/// * every item is a single, non-blank line of at most
///   [`MAX_PLAN_ITEM_BYTES`] bytes;
/// * no item contains a control, bidirectional or invisible character (see
///   [`text::is_unprintable`]) - a plan is drawn on the user's terminal, and
///   an item that rewrites the line it is on is not a step of a plan;
/// * at most one item is `in_progress` - "what am I doing now" has one answer.
///
/// An empty plan is valid: it is how the model says it no longer has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    items: Vec<PlanItem>,
}

impl Plan {
    pub fn new(items: Vec<PlanItem>) -> Result<Self, PlanError> {
        if items.len() > MAX_PLAN_ITEMS {
            return Err(PlanError::TooManyItems {
                count: items.len(),
                limit: MAX_PLAN_ITEMS,
            });
        }

        for (index, item) in items.iter().enumerate() {
            let number = index + 1;
            if item.content.trim().is_empty() {
                return Err(PlanError::BlankItem { number });
            }
            if item.content.contains(['\n', '\r']) {
                return Err(PlanError::MultiLineItem { number });
            }
            if item.content.chars().any(text::is_unprintable) {
                return Err(PlanError::UnprintableItem { number });
            }
            if item.content.len() > MAX_PLAN_ITEM_BYTES {
                return Err(PlanError::ItemTooLong {
                    number,
                    bytes: item.content.len(),
                    limit: MAX_PLAN_ITEM_BYTES,
                });
            }
        }

        let in_progress: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.status == PlanStatus::InProgress)
            .map(|(index, _)| index + 1)
            .collect();
        if in_progress.len() > 1 {
            return Err(PlanError::SeveralInProgress {
                numbers: in_progress,
            });
        }

        Ok(Self { items })
    }

    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn completed(&self) -> usize {
        self.count(PlanStatus::Completed)
    }

    /// True while anything is left to do - the only case in which reminding
    /// the model of the plan is worth the bytes.
    pub fn has_unfinished(&self) -> bool {
        self.completed() < self.items.len()
    }

    fn count(&self, status: PlanStatus) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == status)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: PlanStatus) -> PlanItem {
        PlanItem::new(content, status)
    }

    #[test]
    fn a_well_formed_plan_keeps_its_items_in_order() {
        let plan = Plan::new(vec![
            item("read the parser", PlanStatus::Completed),
            item("add CRLF handling", PlanStatus::InProgress),
            item("run the tests", PlanStatus::Pending),
        ])
        .unwrap();

        let contents: Vec<&str> = plan.items().iter().map(|i| i.content.as_str()).collect();
        assert_eq!(
            contents,
            ["read the parser", "add CRLF handling", "run the tests"]
        );
        assert_eq!(plan.completed(), 1);
        assert!(plan.has_unfinished());
    }

    #[test]
    fn an_empty_plan_is_valid_and_has_nothing_unfinished() {
        let plan = Plan::new(Vec::new()).unwrap();
        assert!(plan.is_empty());
        assert!(!plan.has_unfinished());
    }

    #[test]
    fn a_fully_completed_plan_has_nothing_unfinished() {
        let plan = Plan::new(vec![item("done", PlanStatus::Completed)]).unwrap();
        assert!(!plan.has_unfinished());
    }

    #[test]
    fn at_most_one_item_may_be_in_progress() {
        let error = Plan::new(vec![
            item("a", PlanStatus::InProgress),
            item("b", PlanStatus::Pending),
            item("c", PlanStatus::InProgress),
        ])
        .unwrap_err();
        assert_eq!(
            error,
            PlanError::SeveralInProgress {
                numbers: vec![1, 3]
            }
        );
    }

    #[test]
    fn the_item_limit_is_enforced_rather_than_truncated() {
        let at_limit: Vec<PlanItem> = (0..MAX_PLAN_ITEMS)
            .map(|n| item(&format!("step {n}"), PlanStatus::Pending))
            .collect();
        assert!(Plan::new(at_limit.clone()).is_ok());

        let mut over = at_limit;
        over.push(item("one too many", PlanStatus::Pending));
        assert_eq!(
            Plan::new(over).unwrap_err(),
            PlanError::TooManyItems {
                count: MAX_PLAN_ITEMS + 1,
                limit: MAX_PLAN_ITEMS
            }
        );
    }

    #[test]
    fn blank_items_are_rejected_with_their_position() {
        let error = Plan::new(vec![
            item("fine", PlanStatus::Pending),
            item("   ", PlanStatus::Pending),
        ])
        .unwrap_err();
        assert_eq!(error, PlanError::BlankItem { number: 2 });
    }

    #[test]
    fn multi_line_items_are_rejected_not_joined() {
        for content in ["first\nsecond", "first\r\nsecond", "trailing\n"] {
            let error = Plan::new(vec![item(content, PlanStatus::Pending)]).unwrap_err();
            assert_eq!(error, PlanError::MultiLineItem { number: 1 }, "{content:?}");
        }
    }

    #[test]
    fn the_byte_limit_is_enforced_rather_than_clipped() {
        let at_limit = "x".repeat(MAX_PLAN_ITEM_BYTES);
        assert!(Plan::new(vec![item(&at_limit, PlanStatus::Pending)]).is_ok());

        // Multi-byte characters count by bytes, which is what the budget is in.
        let over = "é".repeat(MAX_PLAN_ITEM_BYTES / 2 + 1);
        assert_eq!(
            Plan::new(vec![item(&over, PlanStatus::Pending)]).unwrap_err(),
            PlanError::ItemTooLong {
                number: 1,
                bytes: over.len(),
                limit: MAX_PLAN_ITEM_BYTES
            }
        );
    }

    #[test]
    fn statuses_use_the_names_the_tool_schema_advertises() {
        let parsed: Vec<PlanStatus> =
            serde_json::from_str(r#"["pending","in_progress","completed"]"#).unwrap();
        assert_eq!(
            parsed,
            [
                PlanStatus::Pending,
                PlanStatus::InProgress,
                PlanStatus::Completed
            ]
        );
        for status in parsed {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().into())
            );
        }
    }

    #[test]
    fn items_that_would_act_on_the_terminal_are_rejected_not_cleaned() {
        for content in [
            "step\x1b[2K one",
            "tab\there",
            "right\u{202E}to left",
            "zero\u{200B}width",
            "c1\u{9B}2J",
        ] {
            let error = Plan::new(vec![
                item("fine", PlanStatus::Pending),
                item(content, PlanStatus::Pending),
            ])
            .unwrap_err();
            assert_eq!(
                error,
                PlanError::UnprintableItem { number: 2 },
                "{content:?}"
            );
        }
    }
}
