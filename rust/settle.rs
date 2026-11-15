//! A request that would otherwise end without a receipt. A web upgrade's
//! Computer reads only the receipt (it starts this process detached and polls
//! for the file), so a run that refuses early, fails before an operation of its
//! own, or is given up on by its supervisor still leaves one. Create-only
//! (`report::create`): an existing receipt is never replaced, so an id that
//! leaked from an old run cannot overwrite that run's result.
use crate::{
    config::Config,
    operation::{self, Record},
    report::{self, FailureCode, Outcome, Receipt},
    request::{Reply, Request},
};

/// How sure this run is that `request` is its own to settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Not established (an early refusal, or an error before the caller was
    /// checked): an id and a `remote:` label may have leaked from an earlier
    /// web upgrade (#proj-raft-computer:#32), whose own run may still be
    /// working without a receipt yet. Settled only while nothing at all was
    /// started for that id on this computer: a run that started anything
    /// first saved its plan, before it could start a service that leaks.
    Unproven,
    /// This run's own: past the caller checks, the supervisor of its workers,
    /// or the run its escaped copy came from.
    Own,
}

/// The line of a request given up on after its operation started.
pub const UNRESOLVED_LINE: &str =
    "The installation could not be completed or recovered. Run the same install command again.";
pub const UNRESOLVED_REASON: &str = "The installation could not be completed or recovered.";

/// Started by the running service for a web upgrade: the service labels it
/// `remote:<its id>`. Only such a request gets a held or failed receipt from
/// here; a terminal run's id stays free to run again.
pub fn from_web(request: &Request) -> bool {
    ["install", "upgrade", "repair"].contains(&request.command.as_str())
        && request.approved_by.strip_prefix("remote:") == Some(request.id.as_str())
}

/// Writes the receipt `reply` stands for, unless `request` already has one.
/// Best effort: a request that cannot be settled stays as it was. Returns the
/// receipt when this call wrote it.
pub fn settle(cfg: &Config, request: &Request, reply: &Reply, claim: Claim) -> Option<Receipt> {
    let receipt = settlement(request, reply, claim, operation::record(cfg, &request.id))?;
    report::create(cfg, receipt).ok().flatten()
}

/// The receipt for `reply`, or None when this request is not one to settle.
pub(crate) fn settlement(
    request: &Request,
    reply: &Reply,
    claim: Claim,
    record: Record,
) -> Option<Receipt> {
    if reply.exit_code == 0
        || reply.id != request.id
        || request.validate().is_err()
        || !["install", "upgrade", "repair"].contains(&request.command.as_str())
        // Held and failed are final: only a web upgrade's run, whose id is
        // never run again. Unresolved stays open to the same request.
        || (reply.exit_code != 3 && !from_web(request))
    {
        return None;
    }
    let (code, reason) = (reply.code?, reply.reason.clone()?);
    let mut receipt = match (record, claim) {
        (Record::None, _) | (Record::Started, Claim::Own) => {
            let outcome = match reply.exit_code {
                1 => Outcome::Failed,
                2 => Outcome::Held,
                _ => Outcome::Unresolved,
            };
            let mut receipt = Receipt::new(
                request,
                request.version.clone(),
                None,
                outcome,
                reply.line.clone(),
            );
            receipt.code = Some(code);
            receipt.reason = Some(reason);
            receipt.next_step = reply.next_step.clone();
            receipt
        }
        // Its operation started: whatever stopped it, the result is not
        // known, and a later run of the same request may still finish it, so
        // the receipt is unresolved and carries the plan's identity (the one
        // `Receipt::finish` compares). A held or failed one would end its
        // recovery (operation::active).
        (
            Record::Planned {
                request: planned,
                target,
                from,
            },
            Claim::Own,
        ) => {
            let mut receipt = if reply.exit_code == 3 {
                let mut receipt = Receipt::new(
                    &planned,
                    Some(target),
                    from,
                    Outcome::Unresolved,
                    reply.line.clone(),
                );
                receipt.code = Some(code);
                receipt.reason = Some(reason);
                receipt
            } else {
                let mut receipt = Receipt::new(
                    &planned,
                    Some(target),
                    from,
                    Outcome::Unresolved,
                    UNRESOLVED_LINE.into(),
                );
                receipt.code = Some(FailureCode::RecoveryUnresolved);
                receipt.reason = Some(UNRESOLVED_REASON.into());
                receipt
                    .detail
                    .insert("requestCode".into(), code.as_str().into());
                receipt
            };
            receipt.next_step = None;
            receipt
        }
        _ => return None,
    };
    if let Some(diagnostic) = &reply.diagnostic {
        receipt
            .detail
            .insert("diagnostic".into(), diagnostic.clone());
    }
    receipt.detail.insert("settledBy".into(), "fallback".into());
    receipt.validate().ok()?;
    Some(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::Presence;
    use std::path::Path;

    fn config(home: &Path) -> Config {
        let state_home = home.join(".raft");
        Config {
            user_home: home.to_path_buf(),
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            state_home,
            install_dir: home.join(".local/bin"),
            binary: home.join(".local/bin/raft-computer"),
            sidecar: home.join(".local/bin/photon_rs_bg.wasm"),
            hands_origin: "https://hands.build".into(),
            hands_app: "raft-computer-cli".into(),
            waiting_caller: None,
        }
    }

    fn web(id: &str) -> Request {
        Request {
            protocol_version: 1,
            id: id.into(),
            command: "upgrade".into(),
            version: Some("1.25.3".into()),
            channel: None,
            yes: false,
            allow_downgrade: false,
            presence: Presence::Unattended,
            approved_by: format!("remote:{id}"),
            recovery_only: false,
            waiting_caller: None,
            escape_failure: None,
        }
    }

    fn held(id: &str) -> Reply {
        Reply::failure(
            id,
            2,
            "Nothing was changed.",
            FailureCode::InstallationHeld,
            "The upgrade was started from inside Raft Computer.",
        )
        .with_diagnostic("computer process 7 is an ancestor")
    }

    #[test]
    fn a_web_refusal_with_nothing_started_is_settled_as_it_was_answered() {
        let receipt = settlement(&web("w1"), &held("w1"), Claim::Unproven, Record::None).unwrap();
        assert_eq!(receipt.outcome, Outcome::Held);
        assert_eq!(receipt.exit_code, 2);
        assert_eq!(receipt.code, Some(FailureCode::InstallationHeld));
        assert_eq!(
            receipt.reason.as_deref(),
            Some("The upgrade was started from inside Raft Computer.")
        );
        assert_eq!(receipt.target_version.as_deref(), Some("1.25.3"));
        assert_eq!(receipt.operation, "upgrade");
        assert_eq!(receipt.approved_by.as_deref(), Some("remote:w1"));
        assert_eq!(
            receipt.detail.get("diagnostic").map(String::as_str),
            Some("computer process 7 is an ancestor")
        );
    }

    #[test]
    fn an_unproven_id_with_anything_started_is_left_alone() {
        // A leaked id whose run is still working: it saved its plan before it
        // could start the service that leaked the id.
        for record in [
            Record::Started,
            Record::Unreadable,
            Record::Planned {
                request: Box::new(web("w1")),
                target: "1.25.3".into(),
                from: None,
            },
        ] {
            assert!(settlement(&web("w1"), &held("w1"), Claim::Unproven, record).is_none());
        }
    }

    #[test]
    fn only_a_web_request_gets_a_final_receipt() {
        let mut local = web("local");
        local.approved_by = "terminal operator".into();
        assert!(settlement(&local, &held("local"), Claim::Own, Record::None).is_none());
        // Another request's label is not this request's.
        let mut other = web("w2");
        other.approved_by = "remote:w1".into();
        assert!(settlement(&other, &held("w2"), Claim::Own, Record::None).is_none());
        let mut status = web("w3");
        status.command = "status".into();
        status.version = None;
        assert!(settlement(&status, &held("w3"), Claim::Own, Record::None).is_none());
        // Unresolved stays open to the same request, so any run's is kept.
        let unresolved = Reply::failure(
            "local",
            3,
            UNRESOLVED_LINE,
            FailureCode::RecoveryUnresolved,
            UNRESOLVED_REASON,
        );
        let receipt = settlement(&local, &unresolved, Claim::Own, Record::None).unwrap();
        assert_eq!(receipt.outcome, Outcome::Unresolved);
    }

    #[test]
    fn a_started_operation_is_settled_unresolved_under_its_plan() {
        let mut planned = web("w1");
        planned.version = None;
        planned.channel = Some("main".into());
        let mut request = planned.clone();
        request.escape_failure = Some("note".into());
        let failed = Reply::failure(
            "w1",
            1,
            "The upgrade stopped. Run the same install command again.",
            FailureCode::EscapeFailed,
            "The upgrade stopped.",
        );
        let receipt = settlement(
            &request,
            &failed,
            Claim::Own,
            Record::Planned {
                request: Box::new(planned),
                target: "1.25.3".into(),
                from: Some("1.0.68".into()),
            },
        )
        .unwrap();
        assert_eq!(receipt.outcome, Outcome::Unresolved);
        assert_eq!(receipt.exit_code, 3);
        assert_eq!(receipt.code, Some(FailureCode::RecoveryUnresolved));
        assert_eq!(receipt.target_version.as_deref(), Some("1.25.3"));
        assert_eq!(receipt.from_version.as_deref(), Some("1.0.68"));
        assert_eq!(
            receipt.detail.get("requestCode").map(String::as_str),
            Some("escape_failed")
        );
    }

    #[test]
    fn create_never_replaces_a_receipt() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let first = settle(&cfg, &web("w1"), &held("w1"), Claim::Own).unwrap();
        assert_eq!(report::read(&cfg, "w1").unwrap().unwrap(), first);
        let failed = Reply::failure(
            "w1",
            1,
            "Failed.",
            FailureCode::UnexpectedError,
            "The installation could not finish.",
        );
        assert!(settle(&cfg, &web("w1"), &failed, Claim::Own).is_none());
        assert_eq!(report::read(&cfg, "w1").unwrap().unwrap(), first);
        // An unreadable receipt is kept too (the request's result is unknown,
        // not absent).
        let path = cfg.receipt_path("w2");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(settle(&cfg, &web("w2"), &failed, Claim::Own).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        // Nothing but the receipts is left in their folder.
        let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn a_settled_request_reads_as_its_own_reply() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let receipt = settle(&cfg, &web("w1"), &held("w1"), Claim::Own).unwrap();
        let reply = Reply::receipt(receipt);
        reply.validate("w1").unwrap();
        assert_eq!(reply.exit_code, 2);
    }

    #[test]
    fn an_unresolved_receipt_without_a_target_is_finished_by_its_continuation() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let mut request = web("w1");
        request.version = None;
        request.channel = Some("main".into());
        let given_up = Reply::failure(
            "w1",
            3,
            UNRESOLVED_LINE,
            FailureCode::RecoveryUnresolved,
            UNRESOLVED_REASON,
        );
        let receipt = settle(&cfg, &request, &given_up, Claim::Own).unwrap();
        assert_eq!(receipt.target_version, None);
        let mut done = Receipt::new(
            &request,
            Some("1.25.3".into()),
            Some("1.0.68".into()),
            Outcome::Promoted,
            "Upgraded.".into(),
        );
        done.detail.clear();
        let done = done.finish(&cfg).unwrap();
        assert_eq!(report::read(&cfg, "w1").unwrap().unwrap(), done);
        // A finished one never is.
        let mut again = Receipt::new(
            &request,
            Some("1.25.3".into()),
            None,
            Outcome::Failed,
            "Failed.".into(),
        );
        again.code = Some(FailureCode::InstallFailed);
        again.reason = Some("x".into());
        assert!(again.finish(&cfg).is_err());
    }
}
