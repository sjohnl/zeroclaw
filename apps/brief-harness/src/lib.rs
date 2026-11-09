//! Brief harness: an autonomous, cited intelligence-brief service built on
//! ZeroClaw agents.
//!
//! The harness keeps every side effect in deterministic code and gives the
//! language models no tools. Models only read framed source data and return
//! structured JSON; this crate decides what is fetched, what is spent, what
//! is true enough to send, and where it goes.
//!
//! Pipeline, per subscription:
//!
//! 1. [`snapshot`]: fetch each allowlisted source of the subscription's
//!    [`pack::VerticalPack`], normalize it to text, hash it, and diff it
//!    against the last snapshot. Unchanged sources stop here, with no model
//!    spend.
//! 2. [`budget`]: before every model call, check that model's month-to-date
//!    spend in ZeroClaw's own cost ledger against its monthly cap.
//! 3. [`a2a`]: call the analyst, writer, and verifier agents through the
//!    ZeroClaw gateway's A2A endpoint.
//! 4. [`verify`]: reject any brief whose claims do not quote, verbatim, the
//!    changed text of a snapshot captured in this run.
//! 5. [`delivery`]: send (autonomous mode), queue for sign-off (approval
//!    mode), or hold. Nothing that failed a gate is ever sent.
//! 6. [`audit`]: every step appends to a hash-chained log.

pub mod a2a;
pub mod approvals;
pub mod audit;
pub mod brief;
pub mod budget;
pub mod config;
pub mod delivery;
pub mod fsutil;
pub mod pack;
pub mod pipeline;
pub mod prompts;
pub mod snapshot;
pub mod verify;
