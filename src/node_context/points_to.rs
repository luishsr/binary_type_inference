use crate::constraint_generation::{PointsToMapping, TypeVariableAccess};
use crate::constraints::TypeVariable;
use anyhow::Result;
use cwe_checker_lib::abstract_domain::{
    AbstractIdentifier, DataDomain, IntervalDomain, TryToBitvec,
};
use cwe_checker_lib::analysis::graph::Graph;
use cwe_checker_lib::analysis::interprocedural_fixpoint_generic::NodeValue;
use cwe_checker_lib::analysis::pointer_inference;
use cwe_checker_lib::intermediate_representation::{ByteSize, Project, Variable};
use cwe_checker_lib::utils::binary::RuntimeMemoryImage;
use log::{error, info, warn};
use petgraph::graph::NodeIndex;
use std::collections::{BTreeSet, HashMap};

/// Holds a pointer_inference state for a node in order to mantain a type variable mapping for pointers.
pub struct PointsToContext {
    pointer_state: pointer_inference::State,
    pub stack_pointer: Variable,
}

impl PointsToContext {
    fn new(st: pointer_inference::State, stack_pointer: Variable) -> PointsToContext {
        PointsToContext {
            pointer_state: st,
            stack_pointer,
        }
    }
}

impl PointsToContext {
    /// Based on this comment in the AbstractObjectList:

    /// Right now this function is only sound if for each abstract object only one ID pointing to it exists.
    /// Violations of this will be detected and result in panics.
    /// Further investigation into the problem is needed
    /// to decide, how to correctly represent and handle cases,
    /// where more than one ID should point to the same object.
    ///

    /// We assume that abstract identifiers are unique.
    fn memory_access_into_tvar(
        &self,
        object_id: &AbstractIdentifier,
        offset: &IntervalDomain,
        sz: ByteSize,
    ) -> TypeVariableAccess {
        // TODO(ian): we may want to normalize this offset to the abstract object offset
        // TODO(ian): This normalizes to the *current* stack size but this could cause overlaps if we have say stack params.
        TypeVariableAccess {
            offset: offset.try_to_offset().ok().and_then(|off| {
                let mut curr_offset = off;
                if &self.pointer_state.stack_id == object_id {
                    // access on our current stack
                    // so safe to normalize to positive
                    if let Some((_stack_id, sp_off)) = self
                        .pointer_state
                        .get_register(&self.stack_pointer)
                        .get_if_unique_target()
                    {
                        if let Ok(curr_size) = sp_off.try_to_offset() {
                            if (-curr_size) < (-curr_offset) {
                                warn!(
                                    "Accessing stack offset {} when size is {}",
                                    curr_offset, curr_size
                                );
                            }
                            info!(
                                "Updating offset {} by {} to {}",
                                curr_offset,
                                -curr_size,
                                curr_offset - curr_size
                            );
                            curr_offset = curr_offset - curr_size;
                        }
                    }
                }

                if curr_offset.is_negative() {
                    warn!(
                        "Unhandled negative offset {:?} {} stack_id: {},",
                        object_id.to_string(),
                        curr_offset,
                        self.pointer_state.stack_id,
                    );
                    None
                } else {
                    Some(curr_offset)
                }
            }),
            ty_var: TypeVariable::new(
                object_id
                    .to_string()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            ),
            sz,
        }
    }

    fn dom_val_to_tvars(
        &self,
        dom_val: &DataDomain<IntervalDomain>,
        sz: ByteSize,
    ) -> BTreeSet<TypeVariableAccess> {
        dom_val
            .get_relative_values()
            .iter()
            .map(|(a_id, offset)| self.memory_access_into_tvar(a_id, offset, sz))
            .collect()
    }
}

impl PointsToMapping for PointsToContext {
    /// This method is conservative and only returns abstract objects for which we have an
    // TODO(ian): we should probably handle conflicting sizes
    fn points_to(
        &self,
        address: &cwe_checker_lib::intermediate_representation::Expression,
        sz: cwe_checker_lib::intermediate_representation::ByteSize,
        _vman: &mut crate::constraints::VariableManager,
    ) -> std::collections::BTreeSet<TypeVariableAccess> {
        let dom_val = self.pointer_state.eval(address);
        self.dom_val_to_tvars(&dom_val, sz)
    }
}

/// Runs analysis on the project to generate a [PointsToMapping]
pub fn run_analysis<'a>(
    proj: &'a Project,
    config: pointer_inference::Config,
    cfg: &'a Graph<'a>,
    rt_mem: &'a RuntimeMemoryImage,
) -> Result<HashMap<NodeIndex, PointsToContext>> {
    let pointer_res = pointer_inference::run(proj, rt_mem, cfg, config, false, false);

    Ok(cfg
        .node_indices()
        .filter_map(|idx| {
            pointer_res.get_node_value(idx).and_then(|nv| match nv {
                NodeValue::CallFlowCombinator {
                    call_stub,
                    interprocedural_flow,
                } => (if interprocedural_flow.is_some() {
                    interprocedural_flow
                } else {
                    call_stub
                })
                .as_ref()
                .map(|v| {
                    (
                        idx,
                        PointsToContext::new(v.clone(), proj.stack_pointer_register.clone()),
                    )
                }),

                NodeValue::Value(v) => Some((
                    idx,
                    PointsToContext::new(v.clone(), proj.stack_pointer_register.clone()),
                )),
            })
        })
        .collect())
}
