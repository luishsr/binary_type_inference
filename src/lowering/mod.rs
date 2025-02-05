use cwe_checker_lib::intermediate_representation::{Arg, Tid};

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{
    constraints,
    ctypes::{self, CTypeMapping},
    solver::type_sketch::LatticeBounds,
};
use std::convert::TryInto;

use serde::{Deserialize, Serialize};

use petgraph::{graph::NodeIndex, visit::EdgeRef, EdgeDirection};

use crate::{
    constraints::FieldLabel,
    solver::{type_lattice::NamedLatticeElement, type_sketch::SketchGraph},
};

use std::collections::BinaryHeap;
use std::convert::TryFrom;

#[derive(Debug, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
/// A unique identifier for a type
pub struct TypeId(usize);

/// Representation of a automata type lowered to a ctype
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum CType {
    /// Primitive means the node has a primitive type associated with its label
    Primitive(String),
    /// A pointer to another ctype
    Pointer {
        /// The target type
        target: TypeId,
    },
    /// An alias to the type of a different node
    Alias(NodeIndex),
    /// Reperesents the fields of a structure. These fields are guarenteed to not overlap, however, may be out of order and require padding.
    Structure(Vec<Field>),
    /// Represents the set of parameters and return type. The parameters may be out of order or missing types. One should consider missing parameters as
    Function {
        /// The parameters of the function
        params: Vec<Parameter>,
        /// The return type of the function
        return_ty: Option<TypeId>,
    },
    /// A union of several ctypes
    Union(BTreeSet<TypeId>),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
/// Represents a parameter at a given index.
pub struct Parameter {
    index: usize,
    type_index: TypeId,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
/// Represents a field with an offset and type.
pub struct Field {
    byte_offset: usize,
    bit_sz: usize,
    type_index: TypeId,
}

#[derive(PartialEq, Eq)]
struct Classroom {
    scheduled: Vec<Field>,
    covering: BTreeMap<usize, usize>,
}

impl Classroom {
    fn new() -> Classroom {
        Classroom {
            scheduled: Vec::new(),
            covering: BTreeMap::new(),
        }
    }

    fn compute_upper_bound_exclusive(base: usize, size: usize) -> usize {
        base + size / 8
    }

    fn compute_fld_upper_bound_exlcusive(fld: &Field) -> usize {
        Classroom::compute_upper_bound_exclusive(fld.byte_offset, fld.bit_sz)
    }

    fn get_next_scheduluable_offset(&self) -> usize {
        self.scheduled
            .last()
            .map(Classroom::compute_fld_upper_bound_exlcusive)
            .unwrap_or(std::usize::MIN)
    }

    fn superscedes_fld(&self, fld: &Field) -> bool {
        let mut overlapping_flds = self
            .covering
            .range(fld.byte_offset..Classroom::compute_fld_upper_bound_exlcusive(fld));

        // we can assume if any fld compeltely contains this fld then this fld is already covered
        overlapping_flds.any(|(overlapping_base, overlapping_size)| {
            *overlapping_base < fld.byte_offset
                && Self::compute_upper_bound_exclusive(*overlapping_base, *overlapping_size)
                    >= Self::compute_fld_upper_bound_exlcusive(fld)
        })
    }

    fn schedule_fld(&mut self, fld: Field) -> bool {
        if self.get_next_scheduluable_offset() > fld.byte_offset {
            return false;
        }

        self.covering.insert(fld.byte_offset, fld.bit_sz);

        self.scheduled.push(fld);
        true
    }
}

impl Ord for Classroom {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // flip ordering to make this a min heap with respect to the last mapped offset
        other
            .get_next_scheduluable_offset()
            .cmp(&self.get_next_scheduluable_offset())
    }
}

impl PartialOrd for Classroom {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn translate_field(field: &constraints::Field, idx: TypeId) -> Option<Field> {
    usize::try_from(field.offset).ok().map(|off| Field {
        byte_offset: off,
        bit_sz: field.size,
        type_index: idx,
    })
}

fn schedule_structures(fields: &[Field]) -> Vec<CType> {
    // So the goal here is to select the minimal partitioning of these fields into structures.
    // Here are the rules:
    // 1. A structure cannot contain two fields that overlap
    // 2. If a field is completely contained within another field we may remove it

    // This is simply interval scheduling with one caveat. If a time block overlaps but is completely contained within the ending field we can just ignore
    // That field
    let mut sorted_fields = fields.to_vec();
    sorted_fields.sort_by_key(|x| x.byte_offset);

    let mut hp: BinaryHeap<Classroom> = BinaryHeap::new();

    // TODO(Ian): this is n squared ish can we do better
    for fld in sorted_fields.iter() {
        // Check if we have to schedule this fld
        if !hp.iter().any(|cls| cls.superscedes_fld(fld)) {
            // Schedule it either in the open room or create a new room.
            let mut scheduled = false;
            if let Some(mut clsroom) = hp.peek_mut() {
                scheduled = clsroom.schedule_fld(fld.clone());
            }

            if !scheduled {
                let mut new_class = Classroom::new();
                let res = new_class.schedule_fld(fld.clone());

                assert!(res);
                hp.push(new_class);
            }
        }
    }

    hp.into_iter()
        .map(|x| CType::Structure(x.scheduled))
        .collect()
}

fn has_non_zero_fields<U: NamedLatticeElement>(
    nd: NodeIndex,
    grph: &SketchGraph<LatticeBounds<U>>,
) -> bool {
    grph.get_graph()
        .get_graph()
        .edges_directed(nd, EdgeDirection::Outgoing)
        .any(|e| {
            if let FieldLabel::Field(fld) = e.weight() {
                fld.offset != 0
            } else {
                false
            }
        })
}

fn build_alias_types<U: NamedLatticeElement>(
    nd: NodeIndex,
    grph: &SketchGraph<LatticeBounds<U>>,
) -> Vec<CType> {
    if has_non_zero_fields(nd, grph) {
        return Vec::new();
    }

    let unique_tgts = grph
        .get_graph()
        .get_graph()
        .edges_directed(nd, EdgeDirection::Outgoing)
        .filter(|e| matches!(e.weight(), FieldLabel::Field(_)))
        .map(|e| e.target())
        .collect::<BTreeSet<_>>();

    unique_tgts.into_iter().map(CType::Alias).collect()
}

fn field_to_protobuf(internal_field: Field) -> ctypes::Field {
    ctypes::Field {
        bit_size: internal_field.bit_sz.try_into().unwrap(),
        byte_offset: internal_field.byte_offset.try_into().unwrap(),
        type_id: Some(convert_typeid(internal_field.type_index)),
    }
}

fn param_to_protofbuf(internal_param: Parameter) -> ctypes::Parameter {
    ctypes::Parameter {
        parameter_index: internal_param.index.try_into().unwrap(),
        type_index: Some(convert_typeid(internal_param.type_index)),
    }
}

/// Converts a type id to protobuf
pub fn convert_typeid(type_id: TypeId) -> ctypes::TypeId {
    ctypes::TypeId {
        type_id: u32::try_from(type_id.0).unwrap(),
    }
}

/// Converts an in memory [CType] to a protobuf representation of the enum
pub fn produce_inner_types(
    ct: CType,
    mp: &HashMap<NodeIndex, TypeId>,
) -> ctypes::c_type::InnerType {
    match ct {
        CType::Alias(tgt) => ctypes::c_type::InnerType::Alias(ctypes::Alias {
            to_type: mp.get(&tgt).map(|tyid| convert_typeid(*tyid)),
        }),
        CType::Function { params, return_ty } => {
            let mut func = ctypes::Function::default();
            params
                .into_iter()
                .for_each(|x| func.parameters.push(param_to_protofbuf(x)));

            if let Some(return_ty) = return_ty {
                func.return_type = Some(convert_typeid(return_ty));
                func.has_return = true;
            } else {
                func.has_return = false;
            }

            ctypes::c_type::InnerType::Function(func)
        }
        CType::Pointer { target } => ctypes::c_type::InnerType::Pointer(ctypes::Pointer {
            to_type_id: Some(convert_typeid(target)),
        }),
        CType::Primitive(val) => {
            ctypes::c_type::InnerType::Primitive(ctypes::Primitive { type_constant: val })
        }
        CType::Structure(fields) => {
            let mut st = ctypes::Structure::default();
            fields
                .into_iter()
                .for_each(|x| st.fields.push(field_to_protobuf(x)));

            ctypes::c_type::InnerType::Structure(st)
        }
        CType::Union(children) => {
            let mut union = ctypes::Union::default();
            children
                .into_iter()
                .for_each(|x| union.target_type_ids.push(convert_typeid(x)));

            ctypes::c_type::InnerType::Union(union)
        }
    }
}

// TODO(ian): dont unwrap u32s
/// Converts a mapping from NodeIndex's to CTypes to a protobuf representation [CTypeMapping].
pub fn convert_mapping_to_profobuf(
    mp: BTreeMap<TypeId, CType>,
    node_to_ty: &HashMap<NodeIndex, TypeId>,
) -> CTypeMapping {
    let mut mapping = CTypeMapping::default();

    mp.into_iter().for_each(|(idx, ctype)| {
        let ctype = produce_inner_types(ctype, node_to_ty);
        mapping.type_id_to_ctype.insert(
            convert_typeid(idx).type_id,
            ctypes::CType {
                type_id: Some(convert_typeid(idx)),
                inner_type: Some(ctype),
            },
        );
    });

    mapping
}

/// The context needed to attempt to lower a node to a ctype.
/// The heuristics need to know the original outparam locations for
/// subprocedure nodes, and a default lattice element to use for unknown types.
pub struct LoweringContext<'a, U: NamedLatticeElement> {
    grph: &'a SketchGraph<LatticeBounds<U>>,
    out_params: BTreeMap<NodeIndex, Vec<Arg>>,
    default_lattice_elem: LatticeBounds<U>,
    ephemeral_types: BTreeMap<TypeId, CType>,
    cached_primitivies: BTreeMap<String, TypeId>,
    curr_id: usize,
}

impl<'a, U: NamedLatticeElement> LoweringContext<'a, U> {
    fn build_structure_types(
        &mut self,
        nd: NodeIndex,
        grph: &SketchGraph<LatticeBounds<U>>,
    ) -> Vec<CType> {
        // check if this is an actual  structure
        if !has_non_zero_fields(nd, grph) {
            return Vec::new();
        }

        schedule_structures(
            &grph
                .get_graph()
                .get_graph()
                .edges_directed(nd, EdgeDirection::Outgoing)
                .filter_map(|e| {
                    if let constraints::FieldLabel::Field(fld) = e.weight() {
                        translate_field(fld, self.add_type(CType::Alias(e.target())))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
        )
    }

    fn build_terminal_type(&mut self, nd_bounds: &LatticeBounds<U>) -> TypeId {
        // TODO(Ian): be more clever
        let nm = nd_bounds.get_upper().get_name();
        if let Some(id) = self.cached_primitivies.get(nm) {
            return *id;
        }

        let ty = CType::Primitive(nm.to_owned());
        let res = self.add_type(ty);
        self.cached_primitivies.insert(nm.to_owned(), res);
        res
    }

    fn build_pointer_types(
        &mut self,
        nd: NodeIndex,
        grph: &SketchGraph<LatticeBounds<U>>,
    ) -> Vec<CType> {
        let load_or_store_targets = grph
            .get_graph()
            .get_graph()
            .edges_directed(nd, EdgeDirection::Outgoing)
            .filter(|e| {
                matches!(e.weight(), FieldLabel::Load) || matches!(e.weight(), FieldLabel::Store)
            })
            .map(|e| e.target())
            .collect::<BTreeSet<_>>();

        load_or_store_targets
            .into_iter()
            .map(|tgt| CType::Pointer {
                target: self.add_type(CType::Alias(tgt)),
            })
            .collect()
    }

    /// Creates a new type lowering context from a mapping from term to node,
    /// a mapping from subprocedure term to out parameters and a defualt lattice element.
    pub fn new<'b>(
        grph: &'b SketchGraph<LatticeBounds<U>>,
        tid_to_node_index: &HashMap<Tid, NodeIndex>,
        out_param_mapping: &HashMap<Tid, Vec<Arg>>,
        default_lattice_elem: LatticeBounds<U>,
    ) -> LoweringContext<'b, U> {
        LoweringContext {
            grph,
            out_params: out_param_mapping
                .iter()
                .filter_map(|(k, v)| tid_to_node_index.get(k).map(|nd_idx| (*nd_idx, v.clone())))
                .collect(),
            default_lattice_elem,
            ephemeral_types: BTreeMap::new(),
            cached_primitivies: BTreeMap::new(),
            curr_id: grph
                .get_graph()
                .get_graph()
                .node_indices()
                .map(|idx| idx.index())
                .max()
                .unwrap_or(0)
                + 1,
        }
    }

    fn add_type(&mut self, ty: CType) -> TypeId {
        let id = self.curr_id;
        self.curr_id += 1;
        let ty_id = TypeId(id);
        self.ephemeral_types.insert(ty_id, ty);
        ty_id
    }

    fn collect_params(
        &mut self,
        nd: NodeIndex,
        grph: &SketchGraph<LatticeBounds<U>>,
        get_label_idx: &impl Fn(&FieldLabel) -> Option<usize>,
    ) -> Vec<Parameter> {
        let in_params: BTreeMap<usize, Vec<NodeIndex>> = grph
            .get_graph()
            .get_graph()
            .edges_directed(nd, EdgeDirection::Outgoing)
            .fold(BTreeMap::new(), |mut acc, elem| {
                if let Some(idx) = get_label_idx(elem.weight()) {
                    acc.entry(idx).or_insert_with(Vec::new).push(elem.target());

                    acc
                } else {
                    acc
                }
            });

        in_params
            .into_iter()
            .filter_map(|(idx, mut types)| {
                if types.is_empty() {
                    return None;
                }

                Some(Parameter {
                    index: idx,
                    type_index: if types.len() == 1 {
                        let ty = types.remove(0);
                        self.add_type(CType::Alias(ty))
                    } else {
                        let utype = CType::Union(
                            types
                                .into_iter()
                                .map(|x| self.add_type(CType::Alias(x)))
                                .collect(),
                        );
                        self.add_type(utype)
                    },
                })
            })
            .collect::<Vec<_>>()
    }

    fn build_return_type_structure(
        &mut self,
        _idx: NodeIndex,
        orig_param_locs: &[Arg],
        params: &[Parameter],
        default_lattice_elem: &LatticeBounds<U>,
    ) -> CType {
        let mp = params
            .iter()
            .map(|x| (x.index, x))
            .collect::<HashMap<_, _>>();

        let mut flds = Vec::new();
        let mut curr_off = 0;
        for (i, arg) in orig_param_locs.iter().enumerate() {
            flds.push(Field {
                byte_offset: curr_off,
                bit_sz: arg.bytesize().as_bit_length(),
                type_index: mp
                    .get(&i)
                    .map(|x| x.type_index)
                    .unwrap_or_else(|| self.build_terminal_type(default_lattice_elem)),
            });
            // TODO(Ian) doesnt seem like there is a non bit length accessor on the private field?
            curr_off += arg.bytesize().as_bit_length() / 8;
        }

        CType::Structure(flds)
    }

    // unions outs and ins at same parameters if we have multiple conflicting params
    fn build_function_types(
        &mut self,
        nd: NodeIndex,
        grph: &SketchGraph<LatticeBounds<U>>,
    ) -> Vec<CType> {
        // index to vector of targets
        let in_params = self.collect_params(nd, grph, &|lbl| {
            if let FieldLabel::In(idx) = lbl {
                Some(*idx)
            } else {
                None
            }
        });

        let mut out_params = self.collect_params(nd, grph, &|lbl| {
            if let FieldLabel::Out(idx) = lbl {
                Some(*idx)
            } else {
                None
            }
        });

        out_params.sort_by_key(|p| p.index);
        let def = vec![];
        let curr_orig_params = self.out_params.get(&nd).unwrap_or(&def);
        // has multiple out params need to pad
        let oparam = if out_params.len() > 1 || curr_orig_params.len() > 1 {
            log::info!("Creating multifield return type");
            let args = self.out_params.get(&nd).unwrap_or(&def).clone();
            let ret_struct = self.build_return_type_structure(
                nd,
                &args,
                &out_params,
                &self.default_lattice_elem.clone(),
            );
            Some(self.add_type(ret_struct))
        } else {
            out_params.get(0).map(|x| x.type_index)
        };

        if !in_params.is_empty() || !out_params.is_empty() {
            vec![CType::Function {
                params: in_params,
                return_ty: oparam,
            }]
        } else {
            Vec::new()
        }
    }

    // We shall always give a type... even if it is undef
    fn build_type(&mut self, nd: NodeIndex, grph: &SketchGraph<LatticeBounds<U>>) -> TypeId {
        let act_graph = grph.get_graph().get_graph();
        if act_graph
            .edges_directed(nd, EdgeDirection::Outgoing)
            .count()
            == 0
        {
            return self.build_terminal_type(&act_graph[nd]);
        }

        let struct_types = self.build_structure_types(nd, grph);
        // alias types, alias and struct are mutually exclusive, by checking if we only have zero fields in both
        let alias_types = build_alias_types(nd, grph);
        // pointer types
        let pointer_types = self.build_pointer_types(nd, grph);

        // function types

        let function_types = self.build_function_types(nd, grph);

        let mut total_types = Vec::new();

        total_types.extend(struct_types);
        total_types.extend(alias_types);
        total_types.extend(pointer_types);
        total_types.extend(function_types);

        if total_types.len() == 1 {
            self.add_type(total_types.into_iter().next().unwrap())
        } else {
            let union = total_types.into_iter().map(|x| self.add_type(x)).collect();
            self.add_type(CType::Union(union))
        }
    }

    // TODO(Ian) newtype typeids

    /// Collects ctypes for a graph
    pub fn collect_ctypes(
        mut self,
    ) -> anyhow::Result<(HashMap<NodeIndex, TypeId>, BTreeMap<TypeId, CType>)> {
        // types are local decisions so we dont care what order types are built in
        let mut types = HashMap::new();
        for nd in self.grph.get_graph().get_graph().node_indices() {
            types.insert(nd, self.build_type(nd, self.grph));
        }

        Ok((types, self.ephemeral_types))
    }
}
