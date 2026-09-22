//! The stable capability interface every provider adapter reports through,
//! per `VISION.md` §12 ("A stable capability interface, with adapters").
//!
//! [`Usage`] is the one shape token and cost reporting take anywhere in the
//! system: attempt evidence, the TUI, and any future export. A figure a
//! provider did not report is `None`, never a substituted `0` — a missing
//! cost and a free run are different facts, and collapsing them would make
//! attempt evidence lie. [`UsageSource`] records why a figure is present or
//! absent, so `Unavailable` is distinguishable from a value the provider
//! actually reported as zero.
//!
//! [`Provider`] is the one interface every adapter (Claude, Codex, the
//! built-in `dummy`, and whatever the backlog adds later) is invoked
//! through. The core never matches on a concrete provider type: it holds
//! providers as `&dyn Provider` or `Box<dyn Provider>` and drives them
//! through [`Provider::capabilities`] and [`Provider::invoke`] alone, which
//! is what keeps adding a provider from touching supervisor logic.

use crate::{Bus, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod dummy;
pub use dummy::{Dummy, Scenario, ScenarioFile, Step, StepOutcome};

/// Token and cost usage for one attempt, as reported by (or recovered for) a
/// provider. Every field is independently optional: a provider may report
/// tokens but not cost, or cost but not cached tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input (prompt) tokens consumed, if known.
    pub input_tokens: Option<u64>,
    /// Output (completion) tokens produced, if known.
    pub output_tokens: Option<u64>,
    /// Tokens served from a prompt cache, if known and applicable.
    pub cached_tokens: Option<u64>,
    /// Cost in US dollars, if known.
    pub cost_usd: Option<f64>,
    /// Where these figures came from, or why they are absent.
    pub source: UsageSource,
}

/// Where a [`Usage`] figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSource {
    /// The provider reported usage directly, structured.
    Provider,
    /// No structured usage was available; figures were parsed out of the
    /// agent's textual output.
    ParsedFromOutput,
    /// No usage figures could be obtained at all.
    Unavailable,
}

/// What a provider adapter supports, detected at startup (`VISION.md` §12).
///
/// A missing capability is `false`, never inferred from a failed call: the
/// supervisor consults this struct up front to decide what to ask a
/// provider for, rather than discovering the answer by having a request
/// rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The provider can be asked to emit machine-parseable structured
    /// output rather than free-form text.
    pub structured_output: bool,
    /// The provider accepts a specific model selection rather than always
    /// using a fixed default.
    pub model_selection: bool,
    /// The provider reports token or cost usage that [`Provider::invoke`]
    /// can surface as [`Outcome::usage`].
    pub usage_telemetry: bool,
}

/// One request to run a provider: the prompt, the model to use (if the
/// provider supports selection), and the working directory the provider's
/// process should run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The prompt given to the provider.
    pub prompt: String,
    /// The model to invoke, if the provider supports
    /// [`Capabilities::model_selection`] and a specific one was requested.
    pub model: Option<String>,
    /// The directory the provider's process should run in.
    pub working_dir: PathBuf,
}

/// What a provider reported after [`Provider::invoke`] returned.
///
/// An `Ok` [`Outcome`] does not itself mean the attempt succeeded: a
/// nonzero `exit_code` is still an `Ok`, since the provider ran and
/// reported a result. `invoke` returns `Err` only when the provider could
/// not be run or its result could not be obtained at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The provider process's exit code.
    pub exit_code: i32,
    /// Everything the provider wrote to standard output.
    pub stdout: String,
    /// Everything the provider wrote to standard error.
    pub stderr: String,
    /// Token and cost usage, if the provider reports
    /// [`Capabilities::usage_telemetry`].
    pub usage: Option<Usage>,
    /// The provider's own session identifier, if it has one. Recorded in
    /// attempt evidence; no correctness path depends on session resume
    /// (`VISION.md` §12).
    pub session_id: Option<String>,
}

/// One provider (agent backend) that ktask can drive: Claude, Codex, the
/// built-in `dummy`, or a future adapter (`VISION.md` §12).
///
/// The core drives every provider through this trait alone, holding it as
/// `&dyn Provider` or `Box<dyn Provider>`; it never matches on a concrete
/// adapter type, which is what lets a new provider be added without
/// touching supervisor logic.
pub trait Provider {
    /// This provider's name, as recorded in attempt evidence and shown in
    /// the TUI.
    fn name(&self) -> &str;

    /// What this provider supports, detected at startup.
    fn capabilities(&self) -> Capabilities;

    /// Runs `inv` against this provider, publishing progress to `bus` if
    /// one is given, and returns what the provider reported.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider could not be invoked or its result
    /// could not be obtained at all; a failure the provider itself reports
    /// (a nonzero exit, a refusal) is a successful `Outcome`, not an `Err`.
    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome>;
}

#[cfg(test)]
mod trait_object {
    use super::*;
    use std::cell::Cell;

    /// A minimal `Provider` used only to prove the trait is usable as
    /// written: object-safe, invocable through `&dyn Provider`, and able to
    /// report every capability and outcome field this task defines.
    struct Recording {
        capabilities: Capabilities,
        invocations: Cell<u32>,
    }

    impl Provider for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
            self.invocations.set(self.invocations.get() + 1);
            if let Some(bus) = bus {
                bus.publish(crate::Event {
                    seq: crate::EventSeq::new(1),
                    ts: time::OffsetDateTime::UNIX_EPOCH,
                    task_id: None,
                    kind: crate::EventKind::AgentOutput {
                        attempt: crate::AttemptId::new(1),
                        stream: crate::Stream::Stdout,
                        text: inv.prompt.clone(),
                    },
                });
            }
            Ok(Outcome {
                exit_code: 0,
                stdout: format!("ran: {}", inv.prompt),
                stderr: String::new(),
                usage: None,
                session_id: Some("session-1".to_string()),
            })
        }
    }

    fn recording(capabilities: Capabilities) -> Recording {
        Recording {
            capabilities,
            invocations: Cell::new(0),
        }
    }

    fn invocation(prompt: &str) -> Invocation {
        Invocation {
            prompt: prompt.to_string(),
            model: None,
            working_dir: PathBuf::from("/tmp/ktask-provider-trait-test"),
        }
    }

    /// The trait must be object-safe: the core holds providers behind
    /// `&dyn Provider` / `Box<dyn Provider>`, never as a generic type
    /// parameter naming a concrete adapter.
    #[test]
    fn provider_is_object_safe_and_invocable_through_a_trait_object() {
        let provider: Box<dyn Provider> = Box::new(recording(Capabilities {
            structured_output: true,
            model_selection: true,
            usage_telemetry: false,
        }));

        assert_eq!(provider.name(), "recording");

        let outcome = provider
            .invoke(&invocation("do the thing"), None)
            .expect("invoke");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "ran: do the thing");
    }

    /// Driving several distinct adapters through one `&dyn Provider`-typed
    /// call site, with no branch on which concrete type each one is,
    /// demonstrates the "nothing in the core matches on a concrete
    /// provider type" requirement directly rather than by inspection.
    #[test]
    fn a_slice_of_mixed_providers_is_driven_through_the_trait_alone() {
        let a = recording(Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        });
        let b = recording(Capabilities {
            structured_output: true,
            model_selection: false,
            usage_telemetry: true,
        });
        let providers: Vec<&dyn Provider> = vec![&a, &b];

        let outcomes: Vec<Outcome> = providers
            .iter()
            .map(|p| p.invoke(&invocation("go"), None).expect("invoke"))
            .collect();

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.exit_code == 0));
        assert_eq!(a.invocations.get(), 1);
        assert_eq!(b.invocations.get(), 1);
    }

    #[test]
    fn capabilities_reports_exactly_what_the_provider_was_built_with() {
        let provider = recording(Capabilities {
            structured_output: true,
            model_selection: false,
            usage_telemetry: true,
        });

        let caps = provider.capabilities();
        assert!(caps.structured_output);
        assert!(!caps.model_selection);
        assert!(caps.usage_telemetry);
    }

    #[test]
    fn invoke_publishes_to_the_bus_when_one_is_given() {
        let provider = recording(Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        });
        let bus = Bus::new(8);
        let mut sub = bus.subscribe();

        provider
            .invoke(&invocation("hello"), Some(&bus))
            .expect("invoke");

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn invoke_runs_without_a_bus() {
        let provider = recording(Capabilities {
            structured_output: false,
            model_selection: false,
            usage_telemetry: false,
        });

        let outcome = provider.invoke(&invocation("hello"), None).expect("invoke");
        assert_eq!(outcome.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn outcome_carries_usage_when_the_provider_reports_it() {
        struct WithUsage;
        impl Provider for WithUsage {
            fn name(&self) -> &'static str {
                "with-usage"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    structured_output: false,
                    model_selection: false,
                    usage_telemetry: true,
                }
            }
            fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
                Ok(Outcome {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    usage: Some(Usage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        cached_tokens: None,
                        cost_usd: Some(0.01),
                        source: UsageSource::Provider,
                    }),
                    session_id: None,
                })
            }
        }

        let provider = WithUsage;
        let outcome = provider.invoke(&invocation("go"), None).expect("invoke");
        assert_eq!(
            outcome.usage.expect("usage").input_tokens,
            Some(10),
            "a capable provider's reported usage must reach the caller unchanged"
        );
    }

    #[test]
    fn invoke_can_report_a_nonzero_exit_as_a_successful_outcome() {
        struct Failing;
        impl Provider for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    structured_output: false,
                    model_selection: false,
                    usage_telemetry: false,
                }
            }
            fn invoke(&self, _inv: &Invocation, _bus: Option<&Bus>) -> Result<Outcome> {
                Ok(Outcome {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "agent refused".to_string(),
                    usage: None,
                    session_id: None,
                })
            }
        }

        let outcome = Failing.invoke(&invocation("go"), None).expect("invoke");
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.stderr, "agent refused");
    }
}

#[cfg(test)]
mod usage {
    use super::*;

    fn all_usage_sources() -> Vec<UsageSource> {
        vec![
            UsageSource::Provider,
            UsageSource::ParsedFromOutput,
            UsageSource::Unavailable,
        ]
    }

    #[test]
    fn usage_source_has_exactly_three_variants() {
        let variants = all_usage_sources();
        assert_eq!(variants.len(), 3);

        // Exhaustive, wildcard-free match: a variant added to `UsageSource`
        // without being listed here fails to compile instead of silently
        // under-counting.
        for source in variants {
            match source {
                UsageSource::Provider
                | UsageSource::ParsedFromOutput
                | UsageSource::Unavailable => {}
            }
        }
    }

    #[test]
    fn every_usage_source_round_trips_through_json() {
        for source in all_usage_sources() {
            let json = serde_json::to_string(&source).expect("serialize");
            let back: UsageSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(source, back);
        }
    }

    #[test]
    fn usage_round_trips_through_json_with_every_figure_present() {
        let usage = Usage {
            input_tokens: Some(120),
            output_tokens: Some(45),
            cached_tokens: Some(30),
            cost_usd: Some(0.0123),
            source: UsageSource::Provider,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        let back: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(usage, back);
    }

    /// The behavior this type exists to guarantee: an unavailable figure is
    /// `None`, not a substituted `0`. A caller that matched on `Some(0)` to
    /// mean "unavailable" would be looking at the wrong field entirely, and
    /// the JSON on the wire makes that observable as `null`, not `0`.
    #[test]
    fn unavailable_usage_has_no_figures_and_serializes_them_as_null_not_zero() {
        let usage = Usage {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cost_usd: None,
            source: UsageSource::Unavailable,
        };

        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.source, UsageSource::Unavailable);

        let json = serde_json::to_value(usage).expect("serialize");
        assert_eq!(json["input_tokens"], serde_json::Value::Null);
        assert_eq!(json["output_tokens"], serde_json::Value::Null);
        assert_eq!(json["cached_tokens"], serde_json::Value::Null);
        assert_eq!(json["cost_usd"], serde_json::Value::Null);
        assert_ne!(json["input_tokens"], serde_json::json!(0));

        let back: Usage = serde_json::from_value(json).expect("deserialize");
        assert_eq!(usage, back);
    }
}
