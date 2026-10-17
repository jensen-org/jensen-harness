use gix::bstr::BStr;
use gix::refs::transaction::PreviousValue;

use crate::error::{PromoteError, git};
use crate::gates::{Cleared, Gates};
use crate::isolation::{Isolation, Provenance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    Already,
    Squashed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promoted {
    pub head: String,
    pub how: Promotion,
}

fn parse(id: &str) -> Result<gix::ObjectId, PromoteError> {
    gix::ObjectId::from_hex(id.as_bytes()).map_err(git)
}

fn message(provenance: &Provenance, verb: &str) -> String {
    format!(
        "jensen: {verb} {node} in run {run}\n\nJensen-Run: {run}\nJensen-Node: {node}\nJensen-Session: {session}\nJensen-Model: {model}\nJensen-Spec: {spec}\n",
        node = provenance.node,
        run = provenance.run,
        session = provenance.session,
        model = provenance.model,
        spec = provenance.spec,
    )
}

impl Isolation {
    #[must_use]
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        let (Ok(ancestor), Ok(descendant), Ok(repo)) =
            (parse(ancestor), parse(descendant), self.open_repo())
        else {
            return false;
        };
        repo.merge_base(ancestor, descendant)
            .is_ok_and(|base| base.is_some_and(|base| base.detach() == ancestor))
    }

    fn tree_of(
        repo: &gix::Repository,
        commit: gix::ObjectId,
    ) -> Result<gix::ObjectId, PromoteError> {
        Ok(repo
            .find_commit(commit)
            .map_err(git)?
            .tree_id()
            .map_err(git)?
            .detach())
    }

    pub fn merge_into_integration(
        &self,
        run: &str,
        cleared: &Cleared,
        gates: &Gates,
        provenance: &Provenance,
    ) -> Result<Promoted, PromoteError> {
        if cleared.run != run {
            return Err(PromoteError::WrongRun {
                reviewed: cleared.run.clone(),
                run: run.to_owned(),
            });
        }
        let repo = self.open_repo()?;
        let name = Self::integration_ref(run);
        let head = self.integration_head(run)?;
        let (head_id, node_id) = (parse(&head)?, parse(&cleared.commit)?);
        let (head_tree, node_tree) = (
            Self::tree_of(&repo, head_id)?,
            Self::tree_of(&repo, node_id)?,
        );
        if head_id == node_id
            || self.is_ancestor(&cleared.commit, &head)
            || (head_tree == node_tree && self.is_ancestor(&cleared.base, &head))
        {
            return Ok(Promoted {
                head,
                how: Promotion::Already,
            });
        }
        if head != cleared.base {
            return Err(PromoteError::StaleReview {
                node: provenance.node.clone(),
                reviewed: cleared.base.clone(),
                current: head,
            });
        }
        let time = format!(
            "{} +0000",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| PromoteError::Io(error.to_string()))?
                .as_secs()
        );
        let signature = gix::actor::SignatureRef {
            name: BStr::new("Jensen Harness"),
            email: BStr::new("harness@jensen.invalid"),
            time: &time,
        };
        let squashable = self.is_ancestor(&head, &cleared.commit);
        let (tree, parents, how, verb) = if squashable {
            (node_tree, vec![head_id], Promotion::Squashed, "work of")
        } else {
            let options =
                gix::merge::commit::Options::from(repo.tree_merge_options().map_err(git)?);
            let labels = gix::merge::blob::builtin_driver::text::Labels {
                ancestor: None,
                current: Some(BStr::new("integration")),
                other: Some(BStr::new(provenance.node.as_str())),
            };
            let mut outcome = repo
                .merge_commits(head_id, node_id, labels, options)
                .map_err(git)?;
            let unresolved = gix::merge::tree::TreatAsUnresolved::default();
            if outcome.tree_merge.has_unresolved_conflicts(unresolved) {
                let files = outcome
                    .tree_merge
                    .conflicts
                    .iter()
                    .filter(|conflict| conflict.is_unresolved(unresolved))
                    .map(|conflict| conflict.ours.location().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(PromoteError::PromotionConflict {
                    node: provenance.node.clone(),
                    files,
                });
            }
            let tree = outcome.tree_merge.tree.write().map_err(git)?.detach();
            (tree, vec![head_id, node_id], Promotion::Merged, "promote")
        };
        let commit = repo
            .new_commit_as(
                signature,
                signature,
                message(provenance, verb),
                tree,
                parents,
            )
            .map_err(git)?
            .id;
        let commit_hex = commit.to_string();
        let review = gates.evaluate(self, &head, &commit_hex)?;
        if !review.passed() {
            let reasons = review
                .blocks
                .iter()
                .map(|finding| format!("{}: {} {}", finding.gate, finding.path, finding.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(PromoteError::MergedResultBlocked {
                node: provenance.node.clone(),
                reasons,
            });
        }
        repo.reference(
            name.as_str(),
            commit,
            PreviousValue::MustExistAndMatch(gix::refs::Target::Object(head_id)),
            "jensen: promotion",
        )
        .map_err(git)?;
        Ok(Promoted {
            head: commit_hex,
            how,
        })
    }
}
