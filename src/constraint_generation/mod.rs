use cwe_checker_lib::{
    analysis::graph::{Graph, Node},
    intermediate_representation::{Arg, Blk, Def, Term},
};
use petgraph::visit::Dfs;

use cwe_checker_lib::intermediate_representation::{ByteSize, Expression, Variable};

use crate::constraints::{
    ConstraintSet, DerivedTypeVar, SubtypeConstraint, TypeVariable, VariableManager,
};

use std::{
    collections::{btree_set::BTreeSet, HashMap},
    ops::Deref,
};

/// Maps a variable (register) to it's representing type variable at this time step in the program. This type variable is some representation of
/// all reaching definitions of this register.
pub trait RegisterMapping {
    /// Creates or returns the type variable representing this register at this program point. Takes a [VariableManager] so it
    /// can create fresh type variables.
    fn access(&self, var: &Variable, vman: &mut VariableManager) -> (TypeVariable, ConstraintSet);
}

/// Maps an address expression and a size to the possible type variables representing the loaded address at this program point.
/// Implementors of this trait effectively act as memory managers for the type inference algorithm.
pub trait PointsToMapping {
    /// Gets the set of type variables this address expression points to.  Takes a [VariableManager] so it
    /// can create fresh type variables.
    fn points_to(
        &self,
        address: Expression,
        sz: ByteSize,
        vman: &mut VariableManager,
    ) -> BTreeSet<TypeVariable>;
}

/// Links formal parameters with the type variable for their actual argument at callsites.
/// Additionally, links actual returns to formal returns at return sites.
pub trait SubprocedureLocators {
    // TODO(ian) may need the subprocedure tid here.
    /// Takes a points-to and register mapping to resolve type variables of inputs and outputs
    fn get_type_variables_and_constraints_for_arg(
        arg: Arg,
        reg: &impl RegisterMapping,
        points_to: &impl PointsToMapping,
        vm: &mut VariableManager,
    ) -> (BTreeSet<TypeVariable>, ConstraintSet);
}

/// Represents the flow-sensitive context needed by flow-insensitive constraint generation to generate type variables and constraints at a given program point.
/// The register mapping provides constraints and type variables to represent a register when it is accessed via some notion of reaching definitions.
/// The PointsToMapping determines the set of a type variables a load or store points to in order to generate constraints.
/// Finally the SubprocedureLocators are used to link actual and formal arguments and returns within constraints.
pub struct NodeContext<R: RegisterMapping, P: PointsToMapping, S: SubprocedureLocators> {
    reg_map: R,
    points_to: P,
    subprocedure_locators: S,
}

impl<R: RegisterMapping, P: PointsToMapping, S: SubprocedureLocators> NodeContext<R, P, S> {
    fn generate_expression_constraint(
        &self,
        lhs_type_var: TypeVariable,
        value: Expression,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        match &value {
            Expression::Var(v2) => {
                let (rhs_type_var, additional_constraints) = self.reg_map.access(&v2, vman);
                let mut s = ConstraintSet::singleton(SubtypeConstraint::new(
                    DerivedTypeVar::new(rhs_type_var),
                    DerivedTypeVar::new(lhs_type_var),
                ));
                s.insert_all(&additional_constraints);
                s
            }
            _ => ConstraintSet::empty(),
        }
    }

    fn apply_assign(&self, var: Variable, value: Expression, vman: &mut VariableManager) {
        let (typ_var, constraints) = self.reg_map.access(&var, vman);
    }

    fn handle_def(&self, df: &Term<Def>, vman: &mut VariableManager) -> ConstraintSet {
        match &df.term {
            Def::Load { var, address } => ConstraintSet::default(),
            Def::Store { address, value } => ConstraintSet::default(),
            Def::Assign { var, value } => ConstraintSet::default(),
        }
    }

    fn handle_block_start(&self, blk: &Term<Blk>, vman: &mut VariableManager) -> ConstraintSet {
        blk.term
            .defs
            .iter()
            .map(|df: &Term<Def>| self.handle_def(df, vman))
            .fold(ConstraintSet::default(), |x, y| {
                ConstraintSet::from(
                    x.union(&y)
                        .cloned()
                        .collect::<BTreeSet<SubtypeConstraint>>(),
                )
            })
    }
}

pub struct Context<'a, R, P, S>
where
    R: RegisterMapping,
    P: PointsToMapping,
    S: SubprocedureLocators,
{
    graph: &'a Graph<'a>,
    node_contexts: HashMap<Node<'a>, NodeContext<R, P, S>>,
    vman: VariableManager,
}

impl<R, P, S> Context<'_, R, P, S>
where
    R: RegisterMapping,
    P: PointsToMapping,
    S: SubprocedureLocators,
{
    fn generate_constraints_for_node(&self, nd: Node) -> ConstraintSet {
        let nd_cont = self.node_contexts.get(&nd);
        if let Some(nd_cont) = nd_cont {
            match nd {
                Node::BlkStart(blk, sub) => nd_cont.handle_block_start(blk, &mut self.vman),
                Node::CallReturn { call, return_ } => ConstraintSet::default(),
                Node::CallSource { source, target } => ConstraintSet::default(),
                // block post conditions arent needed to generate constraints
                Node::BlkEnd(blk, term) => Default::default(),
            }
        } else {
            ConstraintSet::default()
        }
    }

    pub fn generate_constraints(&self) -> ConstraintSet {
        let mut cs: ConstraintSet = Default::default();
        for nd_ind in self.graph.node_indices() {
            cs = ConstraintSet::from(
                cs.union(&self.generate_constraints_for_node(self.graph[nd_ind]))
                    .cloned()
                    .collect::<BTreeSet<SubtypeConstraint>>(),
            );
        }
        cs
    }
}
