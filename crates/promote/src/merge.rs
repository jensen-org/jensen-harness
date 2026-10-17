use gix::bstr::BStr;
use gix::refs::transaction::PreviousValue;

use crate::error::{PromoteError, git};
use crate::isolation::{Isolation, Provenance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    Already,
    FastForward,
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

    pub fn merge_into_integration(
        &self,
        run: &str,
        node_commit: &str,
        provenance: &Provenance,
    ) -> Result<Promoted, PromoteError> {
        let repo = self.open_repo()?;
        let name = Self::integration_ref(run);
        let head = self.integration_head(run)?;
        let (head_id, node_id) = (parse(&head)?, parse(node_commit)?);
        if head_id == node_id || self.is_ancestor(node_commit, &head) {
            return Ok(Promoted {
                head,
                how: Promotion::Already,
            });
        }
        if self.is_ancestor(&head, node_commit) {
            repo.reference(
                name.as_str(),
                node_id,
                PreviousValue::MustExistAndMatch(gix::refs::Target::Object(head_id)),
                "jensen: fast forward promotion",
            )
            .map_err(git)?;
            return Ok(Promoted {
                head: node_commit.to_owned(),
                how: Promotion::FastForward,
            });
        }
        let options = gix::merge::commit::Options::from(repo.tree_merge_options().map_err(git)?);
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: None,
            current: Some(BStr::new("integration")),
            other: Some(BStr::new(provenance.node.as_str())),
        };
        let mut outcome = repo
            .merge_commits(head_id, node_id, labels, options)
            .map_err(git)?;
        let how = gix::merge::tree::TreatAsUnresolved::default();
        if outcome.tree_merge.has_unresolved_conflicts(how) {
            let files = outcome
                .tree_merge
                .conflicts
                .iter()
                .filter(|conflict| conflict.is_unresolved(how))
                .map(|conflict| conflict.ours.location().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PromoteError::PromotionConflict {
                node: provenance.node.clone(),
                files,
            });
        }
        let tree = outcome.tree_merge.tree.write().map_err(git)?.detach();
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| PromoteError::Io(error.to_string()))?
            .as_secs();
        let time = format!("{seconds} +0000");
        let signature = gix::actor::SignatureRef {
            name: BStr::new("Jensen Harness"),
            email: BStr::new("harness@jensen.invalid"),
            time: &time,
        };
        let message = format!(
            "jensen: promote {node} into run {run}\n\nJensen-Run: {run}\nJensen-Node: {node}\nJensen-Spec: {spec}\n",
            node = provenance.node,
            run = provenance.run,
            spec = provenance.spec,
        );
        let merged = repo
            .commit_as(
                signature,
                signature,
                name.as_str(),
                message,
                tree,
                [head_id, node_id],
            )
            .map_err(git)?
            .detach();
        Ok(Promoted {
            head: merged.to_string(),
            how: Promotion::Merged,
        })
    }
}
