//! Deterministic coordinator: Design supplies the complete execution input.
//! Conductor applies it, runs checks, then requests a review. No retries or
//! rewriting of executor input occur in this layer.

use review_app::ReviewConfig;
use review_protocol::{ReviewRequest, ReviewStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const MAX_OUTPUT_CHARS: usize = 16_384;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequest {
    pub schema: String,
    pub repository_path: PathBuf,
    pub execution: executor::Input,
    #[serde(default)]
    pub checks: Vec<Check>,
    pub review: ReviewSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSettings {
    pub requirements: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider_root: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    #[serde(default)]
    pub wall_clock_budget_secs: Option<u64>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
}

#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Passed,
    NeedsDesign,
    Blocked,
}

#[derive(Serialize)]
pub struct CheckResult {
    pub name: String,
    pub program: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Serialize)]
pub struct WorkflowResult {
    pub schema: &'static str,
    pub status: Status,
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<executor::Outcome>,
    pub checks: Vec<CheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowResult {
    fn new(status: Status, phase: &'static str) -> Self {
        Self {
            schema: "conductor.result/v1",
            status,
            phase,
            execution: None,
            checks: Vec::new(),
            review: None,
            error: None,
        }
    }

    pub fn blocked(phase: &'static str, error: impl Into<String>) -> Self {
        let mut result = Self::new(Status::Blocked, phase);
        result.error = Some(error.into());
        result
    }
}

pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Indeterminate,
}

pub struct ReviewEvaluation {
    pub decision: ReviewDecision,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// Production entry point. Review is invoked through the review-app library,
/// not a review-cli subprocess.
pub fn run(request: &WorkflowRequest) -> WorkflowResult {
    run_with_reviewer(request, |review_request, config| {
        let (result, _events, error) = review_app::run_review(review_request, config, None);
        let decision = match result.status {
            ReviewStatus::Approved => ReviewDecision::Approved,
            ReviewStatus::ChangesRequested => ReviewDecision::ChangesRequested,
            ReviewStatus::Indeterminate => ReviewDecision::Indeterminate,
        };
        ReviewEvaluation {
            decision,
            result: serde_json::to_value(&result).ok(),
            error,
        }
    })
}

/// Injection point for deterministic tests. Only the reviewer is substituted;
/// executor and check commands use their real implementations.
pub fn run_with_reviewer<F>(request: &WorkflowRequest, reviewer: F) -> WorkflowResult
where
    F: FnOnce(&ReviewRequest, &ReviewConfig) -> ReviewEvaluation,
{
    if request.schema != "conductor.request/v1" {
        return WorkflowResult::blocked("input", "Unsupported request schema");
    }
    if request.review.requirements.trim().is_empty() {
        return WorkflowResult::blocked("input", "Review requirements must not be empty");
    }
    if request
        .checks
        .iter()
        .any(|check| check.name.trim().is_empty() || check.program.trim().is_empty())
    {
        return WorkflowResult::blocked("input", "Check name and program must not be empty");
    }
    if request.review.max_turns == Some(0)
        || request.review.max_tool_calls == Some(0)
        || request.review.wall_clock_budget_secs == Some(0)
        || request
            .review
            .max_cost_usd
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
    {
        return WorkflowResult::blocked("input", "Invalid review limits");
    }

    let review_request = ReviewRequest {
        schema: "review.request/v1".to_owned(),
        repository_path: request.repository_path.to_string_lossy().into_owned(),
        base_ref: request.review.base_ref.clone().unwrap_or_else(|| "HEAD".into()),
        head_ref: ":working".into(),
        requirements: Some(request.review.requirements.clone()),
    };
    if let Err(error) = review_protocol::validate_request(&review_request) {
        return WorkflowResult::blocked("input", error);
    }

    let mut config = ReviewConfig::default();
    if let Some(model) = &request.review.model {
        config.model = model.clone();
    }
    config.provider_root = request.review.provider_root.clone();
    if let Some(max_turns) = request.review.max_turns {
        config.max_turns = max_turns;
    }
    if let Some(max_tool_calls) = request.review.max_tool_calls {
        config.max_tool_calls = max_tool_calls;
    }
    if let Some(secs) = request.review.wall_clock_budget_secs {
        config.wall_clock_budget = Some(Duration::from_secs(secs));
    }
    config.max_cost_usd = request.review.max_cost_usd;

    let mut output = WorkflowResult::new(Status::Blocked, "execution");
    let applied = executor::apply(&request.repository_path, &request.execution);
    let success = applied.success;
    let error = applied.error.clone();
    output.execution = Some(applied);
    if !success {
        output.status = Status::NeedsDesign;
        output.error = error;
        return output;
    }

    for check in &request.checks {
        output.phase = "verification";
        let result = Command::new(&check.program)
            .args(&check.args)
            .current_dir(&request.repository_path)
            .output();
        match result {
            Ok(result) => {
                let succeeded = result.status.success();
                let exit_code = result.status.code();
                output.checks.push(CheckResult {
                    name: check.name.clone(),
                    program: check.program.clone(),
                    exit_code,
                    stdout: bounded(&result.stdout),
                    stderr: bounded(&result.stderr),
                });
                if !succeeded {
                    output.status = Status::NeedsDesign;
                    output.error = Some(format!("Check '{}' failed", check.name));
                    return output;
                }
            }
            Err(error) => {
                output.error = Some(format!("Cannot start check '{}': {error}", check.name));
                return output;
            }
        }
    }

    output.phase = "review";
    let evaluation = reviewer(&review_request, &config);
    output.review = evaluation.result;
    output.error = evaluation.error;
    output.status = match evaluation.decision {
        ReviewDecision::Approved if output.error.is_none() => Status::Passed,
        ReviewDecision::ChangesRequested => Status::NeedsDesign,
        ReviewDecision::Approved | ReviewDecision::Indeterminate => Status::Blocked,
    };
    output
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(MAX_OUTPUT_CHARS)
        .collect()
}
