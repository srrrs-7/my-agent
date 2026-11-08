//! Decorators over the [`ApprovalGate`] port: decisions about a tool call
//! that are made before - and sometimes instead of - asking a human.
//!
//! They are stacked, outermost first, as
//! `PlanModeGate(HookGate(RuleGate(<the gate that asks>)))`: plan mode
//! refuses before anything else is consulted, an operator's hook sees only
//! calls plan mode let through, and the permission rules decide whether the
//! human is asked at all. Every one of them forwards
//! [`ApprovalGate::authorize_dependency_change`] to the gate it wraps
//! untouched - the trait's default refuses, and a decorator that forgot to
//! forward would silently refuse every dependency change.
//!
//! [`ApprovalGate::reauthorize_write`] - the second look right before a
//! write, against where it lands - is forwarded the same way. A decorator
//! that answered it itself would quietly stop the rules inside from ever
//! looking again, so the trait gives it no default: a missing forward does
//! not compile. Only [`RuleGate`] answers it itself.
//!
//! [`ApprovalGate`]: agent_domain::ports::approval::ApprovalGate
//! [`ApprovalGate::authorize_dependency_change`]: agent_domain::ports::approval::ApprovalGate::authorize_dependency_change
//! [`ApprovalGate::reauthorize_write`]: agent_domain::ports::approval::ApprovalGate::reauthorize_write

mod plan_mode;
mod rules;

pub use plan_mode::{PlanMode, PlanModeGate};
pub use rules::RuleGate;
