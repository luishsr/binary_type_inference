use std::collections::BTreeSet;

use binary_type_inference::{
    constraint_generation,
    constraints::TyConstraint,
    node_context,
    solver::constraint_graph::{RuleContext, FSA},
    util,
};
use clap::{App, Arg};
use cwe_checker_lib::{analysis::pointer_inference::Config, utils::binary::RuntimeMemoryImage};
use petgraph::dot::Dot;
use regex::Regex;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let matches = App::new("json_to_constraints")
        .arg(Arg::with_name("input_bin").required(true).index(1))
        .arg(Arg::with_name("input_json").required(true).index(2))
        .get_matches();

    let input_bin = matches.value_of("input_bin").unwrap();
    let input_json = matches.value_of("input_json").unwrap();

    let bin_bytes = std::fs::read(input_bin).expect("unable to read bin");

    let json_file = std::fs::File::open(input_json).expect("unable to read json");

    let mut ir = util::get_intermediate_representation_for_reader(json_file, &bin_bytes)?;
    log::info!("Retrieved IR");
    ir.normalize().iter().for_each(|v| util::log_cwe_message(v));
    log::info!("Normalized IR");

    let extern_subs = ir.program.term.extern_symbols.keys().cloned().collect();
    let graph = cwe_checker_lib::analysis::graph::get_program_cfg(&ir.program, extern_subs);

    let mut rt_mem = RuntimeMemoryImage::new(&bin_bytes)?;

    log::info!("Created RuntimeMemoryImage");

    if ir.program.term.address_base_offset != 0 {
        // We adjust the memory addresses once globally
        // so that other analyses do not have to adjust their addresses.
        rt_mem.add_global_memory_offset(ir.program.term.address_base_offset);
    }

    let nd_context = node_context::create_default_context(
        &ir,
        &graph,
        Config {
            allocation_symbols: vec![
                "malloc".to_owned(),
                "calloc".to_owned(),
                "xmalloc".to_owned(),
                "realloc".to_owned(),
            ],
            deallocation_symbols: vec!["free".to_owned()],
        },
        &rt_mem,
    )?;

    let ctx =
        constraint_generation::Context::new(&graph, nd_context, &ir.program.term.extern_symbols);
    let constraints = ctx.generate_constraints();

    for cons in constraints.iter() {
        eprintln!("{}", cons);
    }
    eprintln!("done cons");

    let mut interestings = BTreeSet::new();
    let reg = Regex::new(r"sub_(\d+)(@ESP)?").unwrap();
    for cons in constraints.iter() {
        if let TyConstraint::SubTy(s) = cons {
            if reg.is_match(s.rhs.get_base_variable().get_name()) {
                interestings.insert(s.rhs.get_base_variable().clone());
            }

            if reg.is_match(s.lhs.get_base_variable().get_name()) {
                interestings.insert(s.lhs.get_base_variable().clone());
            }
        }
    }

    let context = RuleContext::new(interestings);

    let mut fsa_res = FSA::new(&constraints, &context).unwrap();
    fsa_res.saturate();
    fsa_res.intersect_with_pop_push();

    println!("{}", Dot::new(fsa_res.get_graph()));

    Ok(())
}
