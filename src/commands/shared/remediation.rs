//! The prompts that ask what to do about a broken configuration reference,
//! and the code that applies the answer.
//!
//! [`remediate`] offers a fix for one broken reference at a time, until what
//! the caller named validates or the user stops accepting fixes.
//! [`confirm_removal`] runs before a removal and reports what points at the
//! instance about to be deleted, so the user can take those with it, cancel,
//! or leave them.
//!
//! Reconfigure and remove run the instance's own setup and removal commands,
//! the ones a user would run by hand. Un-enabling is the one repair applied
//! here, dropping an id from a launcher's `enabled_capabilities`.
//!
use std::collections::HashMap;

use anyhow::Result;

use crate::commands::{CapabilityCommands, LauncherCommands, ModelCommands, ProviderCommands};
use crate::config::Config;
use crate::config::validation::{
    Problem, RefKind, ValidationError, dependents, find_dangling, type_name, validate_ref,
};

/*-- public --------------------------------------------------------------------*/

/// What declining a fix means for the command that asked, which decides only
/// how the last choice is worded. The caller acts on the returned
/// [`Outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnDecline {
    /// The command carries on with the instance left as it is, which is what
    /// an info or detail command does.
    Skip,
    /// The command cannot run against a broken configuration and stops,
    /// which is what `launch` does.
    Abort,
}

/// Whether what the caller named validates now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing was broken, or everything broken was fixed.
    Clean,
    /// Something is still broken: the user declined to fix it, or there was
    /// nobody to ask.
    Unresolved,
}

/// The note a list command puts against each instance of `kind` whose
/// references do not resolve, keyed by instance id.
///
/// A list reports that a problem exists and never prompts about it. Acting on
/// it is left to a command the user chooses to run next.
pub(crate) fn dangling_notes(ctx: &crate::AppContext, kind: RefKind) -> HashMap<String, String> {
    find_dangling(kind, &ctx.config)
        .into_iter()
        .map(|dangling| {
            (
                dangling.instance_id,
                ctx.ui.warn_mark(&format!("⚠ {}", dangling.reason)),
            )
        })
        .collect()
}

/// A selection prompt that names the instance currently configured, and says
/// so when it no longer resolves.
///
/// A setup run over an existing instance offers its current values as
/// defaults. Without this, pressing Enter through the wizard re-saves a
/// dangling reference with nothing on screen to say it was one.
pub(crate) fn prompt_with_current(
    ctx: &crate::AppContext,
    prompt: &str,
    kind: RefKind,
    current: Option<&str>,
) -> String {
    let Some(current) = current.filter(|id| !id.is_empty()) else {
        return prompt.to_string();
    };

    match validate_ref(kind, current, &ctx.config) {
        Ok(()) => format!("{prompt} [current: '{current}']"),
        Err(_) => format!(
            "{prompt} [current: '{current}', {} no longer resolves]",
            ctx.ui.warn_mark("⚠")
        ),
    }
}

/// What a removal should do about the instances pointing at what is being
/// removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Removal {
    /// Go ahead, taking these instances with it. Empty when nothing pointed
    /// at the target, or when the user chose to strand what did.
    Proceed { with: Vec<(RefKind, String)> },
    /// Leave everything alone.
    Cancel,
}

/// Asks what to do about the instances that point at `(kind, id)` before a
/// removal strands them.
///
/// Nothing pointing at it means no prompt. A session with nobody to ask
/// removes only what was asked for, after saying what that breaks.
pub(crate) fn confirm_removal(ctx: &crate::AppContext, kind: RefKind, id: &str) -> Result<Removal> {
    let stranded = dependents(kind, id, &ctx.config);
    if stranded.is_empty() {
        return Ok(Removal::Proceed { with: stranded });
    }

    ctx.ui.warn(&format!("Removing {kind} '{id}' will break:"));
    for (dependent_kind, dependent_id) in &stranded {
        let type_suffix = type_name(*dependent_kind, dependent_id, &ctx.config)
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        ctx.ui.info(&format!(
            "  - {dependent_kind} '{dependent_id}'{type_suffix}"
        ));
    }

    if !ctx.ui.is_interactive() {
        ctx.ui.warn(&format!(
            "Removing only {kind} '{id}'. What depended on it needs fixing."
        ));
        return Ok(Removal::Proceed { with: Vec::new() });
    }

    let together = match stranded.as_slice() {
        [(dependent_kind, dependent_id)] => {
            format!("Remove {kind} '{id}' and {dependent_kind} '{dependent_id}' together")
        }
        _ => format!(
            "Remove {kind} '{id}' and the {} instances that depend on it",
            stranded.len()
        ),
    };
    let items = vec![
        together,
        format!("Cancel, keep {kind} '{id}'"),
        format!("Remove only {kind} '{id}', fix the rest later"),
    ];

    // Cancelling is the default: this is the destructive prompt, and the
    // other two answers both delete something.
    match ctx.ui.select("What would you like to do?", &items, 1)? {
        0 => Ok(Removal::Proceed { with: stranded }),
        2 => Ok(Removal::Proceed { with: Vec::new() }),
        _ => Ok(Removal::Cancel),
    }
}

/// Removes each instance through its own removal command, so anything
/// depending on *them* gets the same question in turn.
pub(crate) fn remove_all(ctx: &mut crate::AppContext, ids: &[(RefKind, String)]) -> Result<()> {
    for (kind, id) in ids {
        remove(ctx, *kind, id)?;
    }
    Ok(())
}

/// Validates `(kind, id)` and offers a fix for whatever is broken,
/// re-validating after each one so a repair that exposes a second problem is
/// offered in turn.
///
/// `may_prompt` is the caller's own mode, false for a non-prompting run such
/// as `setup --auto`. Prompting also needs a `Ui` with somebody to ask, so a
/// JSON or Markdown session never reaches a prompt whatever the caller
/// passes. Without prompting the problem is reported and left alone, which is
/// what skipping does.
///
/// Reached through a launcher, the removal on offer is disabling: the
/// launcher stops enabling the capability and the capability itself stays
/// configured. Deleting an instance is offered only to a caller that named
/// that instance, since a capability may be enabled by more than one launcher.
///
/// The loop ends when validation comes back clean, when the user declines,
/// or when every repair on offer has been tried against the same problem. A
/// repair that changes nothing is dropped from the choices rather than ending
/// the run, so it always terminates and always leaves the remaining repairs
/// reachable.
pub(crate) async fn remediate(
    ctx: &mut crate::AppContext,
    kind: RefKind,
    id: &str,
    on_decline: OnDecline,
    may_prompt: bool,
) -> Result<Outcome> {
    let prompting = may_prompt && ctx.ui.is_interactive();
    let mut previous: Option<ValidationError> = None;
    let mut tried: Vec<Choice> = Vec::new();

    loop {
        let Err(error) = validate_ref(kind, id, &ctx.config) else {
            return Ok(Outcome::Clean);
        };

        // A repair that left the problem exactly as it was will do so again,
        // so it is dropped from the choices rather than ending the run. A
        // reconfiguration the user walked out of, or used to change something
        // else, comes back to the same question with the rest still on offer.
        // A different problem starts over with all of them.
        if previous.as_ref() != Some(&error) {
            tried.clear();
        }

        let Some(fix) = Fix::for_error(&error, &ctx.config, (kind, id)) else {
            // The instance the caller asked about is itself missing, so
            // there is nothing to offer: reconfiguring or removing needs
            // something that exists.
            ctx.ui.warn(&error.to_string());
            return Ok(Outcome::Unresolved);
        };

        if !prompting {
            ctx.ui.warn(&error.to_string());
            return Ok(Outcome::Unresolved);
        }

        // Every repair has been tried and the problem is still here.
        let Some(choice) = choose(ctx, &error, &fix, on_decline, &tried)? else {
            ctx.ui.warn(&format!("Still unresolved: {error}"));
            return Ok(Outcome::Unresolved);
        };

        tried.push(choice);
        match choice {
            Choice::Reconfigure => {
                previous = Some(error);
                reconfigure(ctx, &fix).await?;
            }
            Choice::Remove => {
                previous = Some(error);
                remove(ctx, fix.kind, &fix.id)?;
            }
            Choice::Disable => {
                previous = Some(error);
                disable(ctx, &fix)?;
            }
            // The walk is deterministic, so the next pass would report the
            // problem just declined. Stop rather than ask about it again.
            Choice::Decline => return Ok(Outcome::Unresolved),
        }
    }
}

/*-- private -------------------------------------------------------------------*/

/// The instance a fix acts on, and what can be done to it.
#[derive(Debug, PartialEq, Eq)]
struct Fix {
    kind: RefKind,
    id: String,
    /// The instance's `*_type`, which reconfiguring hands back to setup.
    type_name: String,
    /// False when the type name is itself the problem. Setup cannot run a
    /// type the registry does not have, so removal is the only fix.
    can_reconfigure: bool,
    /// The `(launcher, capability)` pair to disable, when remediation was
    /// reached through a launcher that enables the capability. Some means the
    /// removal on offer drops the id from that launcher's list rather than
    /// deleting the instance.
    disable: Option<(String, String)>,
}

impl Fix {
    fn for_error(error: &ValidationError, config: &Config, root: (RefKind, &str)) -> Option<Self> {
        // An instance that is not configured cannot be acted on, so the fix
        // belongs to whoever points at it: `launch claude` finding that
        // `chat`'s model is gone reconfigures `chat`. Every other problem is
        // a property of the target itself.
        let (kind, id) = match &error.problem {
            Problem::NotConfigured => error.referrer.clone()?,
            _ => error.target.clone(),
        };

        Some(Self {
            type_name: type_name(kind, &id, config)?.to_string(),
            can_reconfigure: !matches!(error.problem, Problem::UnknownType { .. }),
            disable: disable_target(error, kind, &id, root, config),
            kind,
            id,
        })
    }
}

/// The `(launcher, capability)` pair a fix reached through a launcher can
/// disable.
///
/// A capability is shared: other launchers may enable the same instance, and
/// the caller asked to launch one launcher rather than to change the
/// configuration at large. Dropping the id from that launcher's own list
/// repairs what was asked about and leaves everything else alone.
///
/// Two shapes reach here. The launcher enables a capability whose own
/// reference is broken, where the fix acts on that capability; and the
/// launcher enables a capability that is not configured at all, where the fix
/// acts on the launcher.
fn disable_target(
    error: &ValidationError,
    kind: RefKind,
    id: &str,
    root: (RefKind, &str),
    config: &Config,
) -> Option<(String, String)> {
    if root.0 != RefKind::Launcher {
        return None;
    }
    let launcher_id = root.1;

    let capability_id = match kind {
        RefKind::Capability => id,
        RefKind::Launcher if error.target.0 == RefKind::Capability => error.target.1.as_str(),
        _ => return None,
    };

    config
        .get_launcher(launcher_id)?
        .enabled_capabilities
        .iter()
        .any(|enabled| enabled == capability_id)
        .then(|| (launcher_id.to_string(), capability_id.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Reconfigure,
    Remove,
    Disable,
    Decline,
}

/// Reports the problem and asks what to do about it. Declining is the
/// default, so a user who answers without reading changes nothing.
fn choose(
    ctx: &crate::AppContext,
    error: &ValidationError,
    fix: &Fix,
    on_decline: OnDecline,
    tried: &[Choice],
) -> Result<Option<Choice>> {
    let mut choices = Vec::new();
    let mut items = Vec::new();

    if fix.can_reconfigure && !tried.contains(&Choice::Reconfigure) {
        choices.push(Choice::Reconfigure);
        items.push(format!("Reconfigure {} '{}' now", fix.kind, fix.id.clone()));
    }

    match &fix.disable {
        Some((launcher_id, capability_id)) if !tried.contains(&Choice::Disable) => {
            choices.push(Choice::Disable);
            items.push(format!(
                "Remove capability '{capability_id}' from launcher '{launcher_id}'"
            ));
        }
        None if !tried.contains(&Choice::Remove) => {
            choices.push(Choice::Remove);
            items.push(format!("Remove {} '{}'", fix.kind, fix.id));
        }
        _ => {}
    }

    if choices.is_empty() {
        return Ok(None);
    }

    choices.push(Choice::Decline);
    items.push(match on_decline {
        OnDecline::Skip => format!("Skip for now, '{}' stays broken until fixed", fix.id),
        OnDecline::Abort => "Cancel".to_string(),
    });

    ctx.ui.warn(&format!("Configuration issue: {error}"));
    let picked = ctx
        .ui
        .select("What would you like to do?", &items, items.len() - 1)?;

    Ok(Some(choices[picked]))
}

/// Runs the instance's own setup command against the instance, which is what
/// the user would run by hand to change what it points at.
async fn reconfigure(ctx: &mut crate::AppContext, fix: &Fix) -> Result<()> {
    let (kind, type_name, id) = (fix.kind, fix.type_name.as_str(), Some(fix.id.as_str()));
    match kind {
        RefKind::Launcher => LauncherCommands::setup(ctx, type_name, id).await,
        RefKind::Capability => CapabilityCommands::setup(ctx, type_name, id).await,
        RefKind::Model => ModelCommands::setup(ctx, type_name, id).await,
        RefKind::Provider => ProviderCommands::setup(ctx, type_name, id).await,
    }
}

/// Drops the capability from the launcher's `enabled_capabilities`. The
/// capability stays configured, so any other launcher enabling it is
/// untouched.
fn disable(ctx: &mut crate::AppContext, fix: &Fix) -> Result<()> {
    let Some((launcher_id, capability_id)) = fix.disable.clone() else {
        return Ok(());
    };
    // The in-memory change lands either way, which is what the walk about to
    // re-run reads. A failure to persist is reported the way the removal
    // commands report theirs.
    if let Err(e) = ctx.config.update_launcher(&launcher_id, |launcher| {
        launcher
            .enabled_capabilities
            .retain(|id| id != &capability_id)
    }) {
        ctx.ui.warn(&format!(
            "failed to persist the change to '{launcher_id}': {e}"
        ));
    }
    ctx.ui.info(&format!(
        "Launcher '{launcher_id}' no longer enables capability '{capability_id}'."
    ));
    Ok(())
}

fn remove(ctx: &mut crate::AppContext, kind: RefKind, id: &str) -> Result<()> {
    match kind {
        RefKind::Launcher => LauncherCommands::remove(ctx, id),
        RefKind::Capability => CapabilityCommands::remove(ctx, id),
        RefKind::Model => ModelCommands::remove(ctx, id),
        RefKind::Provider => ProviderCommands::remove(ctx, id),
    }
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, LauncherConfig, ModelConfig, ProviderConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn capture(ctx: &crate::AppContext) -> &CaptureUi {
        (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .expect("test contexts are built with a CaptureUi")
    }

    /// Answers the remediation prompts in order. `CaptureUi` falls back to
    /// the prompt's own default once the queue is empty, which is declining.
    fn answer(ctx: &crate::AppContext, choices: &[usize]) {
        let ui = capture(ctx);
        for choice in choices {
            ui.select_answers.borrow_mut().push_back(*choice);
        }
    }

    fn prompts(ctx: &crate::AppContext) -> Vec<(String, Vec<String>)> {
        capture(ctx)
            .select_prompts
            .borrow()
            .iter()
            .map(|(prompt, items, _)| (prompt.clone(), items.clone()))
            .collect()
    }

    /// Launcher `claude` enables capability `chat`, which points at a model
    /// that is not configured. One healthy model satisfies `agent-model`'s
    /// Chat requirement, so reconfiguring `chat` picks it without a prompt
    /// of its own.
    fn ctx_with_a_dangling_model_ref() -> crate::AppContext {
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        };
        ctx.config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        ctx.config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "ollama".to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        ctx.config.capabilities.insert(
            "chat".to_string(),
            CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "gone" }),
            },
        );
        ctx.config.launchers.insert(
            "claude".to_string(),
            LauncherConfig {
                launcher_id: "claude".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec!["chat".to_string()],
                config: serde_json::json!({}),
            },
        );
        ctx
    }

    /// Capability `chat` uses the one configured model, and nothing enables
    /// the capability, so removing it strands nothing further.
    fn ctx_model_with_one_dependent() -> crate::AppContext {
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config.launchers.clear();
        ctx.config.capabilities.get_mut("chat").unwrap().config =
            serde_json::json!({ "model_id": "granite-3.1-8b-instruct" });
        ctx
    }

    #[test]
    fn removing_a_model_with_its_dependent_removes_both() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[0]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();

        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config.get_capability("chat").is_none());
    }

    #[test]
    fn cancelling_a_removal_keeps_both() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[1]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();

        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config.get_capability("chat").is_some());
    }

    #[test]
    fn removing_only_what_was_asked_leaves_the_dependent_broken() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[2]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();

        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_none());
        // Left in place, and now dangling, which `capability list` reports.
        assert!(ctx.config.get_capability("chat").is_some());
        assert!(!find_dangling(RefKind::Capability, &ctx.config).is_empty());
    }

    #[test]
    fn a_session_with_nobody_to_ask_removes_only_what_was_asked() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config.get_capability("chat").is_some());
        // The user is told what was broken even though nothing was asked.
        let warns = capture(&ctx).warns.borrow().clone();
        assert!(warns.iter().any(|w| w.contains("will break")), "{warns:?}");
    }

    #[test]
    fn removing_something_nothing_depends_on_does_not_prompt() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        CapabilityCommands::remove(&mut ctx, "chat").unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config.get_capability("chat").is_none());
    }

    #[tokio::test]
    async fn a_healthy_instance_is_clean_without_prompting() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        assert!(prompts(&ctx).is_empty());
    }

    #[tokio::test]
    async fn reconfigure_runs_setup_against_the_instance_holding_the_reference() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);
        // Setup asks its own "already configured, overwrite?" confirmation
        // on top of the choice made here. Declining it leaves the reference
        // broken, which the loop then reports rather than asking again.
        capture(&ctx).confirm_answers.borrow_mut().push_back(true);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        // Setup ran against `chat`, the capability holding the broken
        // reference, not against the model that is missing.
        assert_eq!(
            ctx.config
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct")
        );
        // And the loop re-validated afterwards rather than taking the fix on
        // trust.
        assert_eq!(outcome, Outcome::Clean);

        let (_, items) = &prompts(&ctx)[0];
        assert!(
            items[0].contains("Reconfigure capability 'chat'"),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn remove_deletes_the_instance_holding_the_reference() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        // Remove, then keep the launcher that enables `chat` when the
        // removal asks about it.
        answer(&ctx, &[1, 2]);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert!(ctx.config.get_capability("chat").is_none());
        // What the caller asked about is gone, so it does not validate.
        assert_eq!(outcome, Outcome::Unresolved);
    }

    #[tokio::test]
    async fn a_fix_that_exposes_a_second_problem_is_offered_in_turn() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        // A second broken capability, so repairing the first leaves one more
        // for the loop to find.
        ctx.config.capabilities.insert(
            "vision".to_string(),
            CapabilityConfig {
                capability_id: "vision".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "also-gone" }),
            },
        );
        ctx.config
            .launchers
            .get_mut("claude")
            .unwrap()
            .enabled_capabilities
            .push("vision".to_string());
        // Un-enable the first, then decline the second.
        answer(&ctx, &[1, 2]);

        let outcome = remediate(&mut ctx, RefKind::Launcher, "claude", OnDecline::Skip, true)
            .await
            .unwrap();

        let prompts = prompts(&ctx);
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(prompts[0].1[0].contains("capability 'chat'"), "{prompts:?}");
        assert!(
            prompts[1].1[0].contains("capability 'vision'"),
            "{prompts:?}"
        );
        assert_eq!(outcome, Outcome::Unresolved);
    }

    #[tokio::test]
    async fn a_launch_un_enables_a_capability_instead_of_deleting_it() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(
            items[1], "Remove capability 'chat' from launcher 'claude'",
            "{items:?}"
        );
        assert_eq!(outcome, Outcome::Clean);
        assert!(
            ctx.config
                .get_launcher("claude")
                .unwrap()
                .enabled_capabilities
                .is_empty()
        );
        assert!(
            ctx.config.get_capability("chat").is_some(),
            "the capability stays configured for any other launcher"
        );
    }

    #[tokio::test]
    async fn a_launch_un_enables_a_capability_that_is_not_configured() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config.capabilities.remove("chat");
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        // The fix acts on the launcher here, and the removal on offer is still
        // the entry in its list rather than the launcher itself.
        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(
            items[1], "Remove capability 'chat' from launcher 'claude'",
            "{items:?}"
        );
        assert_eq!(outcome, Outcome::Clean);
        assert!(
            ctx.config
                .get_launcher("claude")
                .unwrap()
                .enabled_capabilities
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_caller_naming_the_capability_is_still_offered_deletion() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items[1], "Remove capability 'chat'", "{items:?}");
    }

    #[tokio::test]
    async fn a_fix_that_changes_nothing_is_not_offered_again() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);
        // Setup asks its own "already configured, overwrite?" confirmation.
        // Declining it returns having changed nothing, which is the repair
        // that leaves the same problem behind.
        capture(&ctx).confirm_answers.borrow_mut().push_back(false);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        // The same problem comes back, without the repair that just changed
        // nothing. The answer queue is empty by then, so the second prompt
        // takes its default, which declines.
        let prompts = prompts(&ctx);
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(prompts[0].1[0].starts_with("Reconfigure"), "{prompts:?}");
        assert!(
            !prompts[1].1.iter().any(|i| i.starts_with("Reconfigure")),
            "the repair that changed nothing is gone: {prompts:?}"
        );
        assert!(
            prompts[1].1[0].starts_with("Remove"),
            "the other repair is still reachable: {prompts:?}"
        );
        assert_eq!(outcome, Outcome::Unresolved);
        assert_eq!(
            ctx.config
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("gone"),
            "a declined overwrite leaves the configuration alone"
        );
    }

    #[tokio::test]
    async fn a_launch_can_still_un_enable_after_a_reconfiguration_changed_nothing() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        // Reconfigure, walk out of the overwrite, then take the repair that
        // is still on offer rather than having to re-run the command.
        answer(&ctx, &[0, 0]);
        capture(&ctx).confirm_answers.borrow_mut().push_back(false);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let prompts = prompts(&ctx);
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert_eq!(
            prompts[1].1[0], "Remove capability 'chat' from launcher 'claude'",
            "{prompts:?}"
        );
        assert_eq!(outcome, Outcome::Clean);
        assert!(
            ctx.config
                .get_launcher("claude")
                .unwrap()
                .enabled_capabilities
                .is_empty()
        );
    }

    #[tokio::test]
    async fn declining_stops_instead_of_asking_again() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert_eq!(prompts(&ctx).len(), 1);
        assert_eq!(
            ctx.config
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("gone"),
            "declining leaves the configuration alone"
        );
    }

    #[tokio::test]
    async fn a_non_prompting_caller_never_reaches_a_prompt() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        let outcome = remediate(
            &mut ctx,
            RefKind::Capability,
            "chat",
            OnDecline::Skip,
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert!(prompts(&ctx).is_empty());
        assert!(!capture(&ctx).warns.borrow().is_empty(), "still reported");
    }

    #[tokio::test]
    async fn a_non_interactive_session_never_reaches_a_prompt() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert!(prompts(&ctx).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_type_offers_removal_but_not_reconfiguration() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config
            .models
            .get_mut("granite-3.1-8b-instruct")
            .unwrap()
            .model_type = "not-a-model".to_string();
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        // Setup cannot run a type the registry does not have, so the only
        // fix offered is removal.
        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items[0].starts_with("Remove model"), "{items:?}");
        assert_eq!(outcome, Outcome::Unresolved);
    }

    // The `launch` pre-launch is thin policy over `remediate`, so its two
    // tests live here with the fixture rather than in `launcher.rs`.

    #[tokio::test]
    async fn the_launch_prelaunch_aborts_when_the_user_declines() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        // No canned answer, so the prompt takes its default, which declines.
        let result = crate::commands::LauncherCommands::prelaunch(&mut ctx, "claude").await;

        assert!(result.is_err(), "declining must stop the launch");
        assert_eq!(
            result.unwrap_err().to_string(),
            "Launch aborted: launcher 'claude' has a configuration problem that was not fixed."
        );
    }

    #[tokio::test]
    async fn the_launch_prelaunch_proceeds_once_the_reference_is_repaired() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);
        capture(&ctx).confirm_answers.borrow_mut().push_back(true);

        crate::commands::LauncherCommands::prelaunch(&mut ctx, "claude")
            .await
            .expect("a repaired configuration launches");

        assert_eq!(
            ctx.config
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct")
        );
    }

    #[tokio::test]
    async fn aborting_callers_are_offered_cancel_rather_than_skip() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items[2], "Cancel", "{items:?}");
    }
}
