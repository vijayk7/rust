// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Trivial copy propagation pass.
//!
//! This uses def-use analysis to remove values that have exactly one def and one use, which must
//! be an assignment.
//!
//! To give an example, we look for patterns that look like:
//!
//!     DEST = SRC
//!     ...
//!     USE(DEST)
//!
//! where `DEST` and `SRC` are both locals of some form. We replace that with:
//!
//!     NOP
//!     ...
//!     USE(SRC)
//!
//! The assignment `DEST = SRC` must be (a) the only mutation of `DEST` and (b) the only
//! (non-mutating) use of `SRC`. These restrictions are conservative and may be relaxed in the
//! future.

use rustc::hir;
use rustc::mir::{Constant, Local, LocalKind, Location, Lvalue, Mir, Operand, Rvalue, StatementKind};
use rustc::mir::visit::MutVisitor;
use rustc::ty::TyCtxt;
use transform::{MirPass, MirSource};
use util::def_use::DefUseAnalysis;

pub struct CopyPropagation;

impl MirPass for CopyPropagation {
    fn run_pass<'a, 'tcx>(&self,
                          tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          source: MirSource,
                          mir: &mut Mir<'tcx>) {
        // Don't run on constant MIR, because trans might not be able to
        // evaluate the modified MIR.
        // FIXME(eddyb) Remove check after miri is merged.
        let id = tcx.hir.as_local_node_id(source.def_id).unwrap();
        match (tcx.hir.body_owner_kind(id), source.promoted) {
            (_, Some(_)) |
            (hir::BodyOwnerKind::Const, _) |
            (hir::BodyOwnerKind::Static(_), _) => return,

            (hir::BodyOwnerKind::Fn, _) => {
                if tcx.is_const_fn(source.def_id) {
                    // Don't run on const functions, as, again, trans might not be able to evaluate
                    // the optimized IR.
                    return
                }
            }
        }

        // We only run when the MIR optimization level is > 1.
        // This avoids a slow pass, and messing up debug info.
        if tcx.sess.opts.debugging_opts.mir_opt_level <= 1 {
            return;
        }

        let mut def_use_analysis = DefUseAnalysis::new(mir);
        loop {
            def_use_analysis.analyze(mir);

            if eliminate_self_assignments(mir, &def_use_analysis) {
                def_use_analysis.analyze(mir);
            }

            let mut changed = false;
            for dest_local in mir.local_decls.indices() {
                debug!("Considering destination local: {:?}", dest_local);

                let action;
                let location;
                {
                    // The destination must have exactly one def.
                    let dest_use_info = def_use_analysis.local_info(dest_local);
                    let dest_def_count = dest_use_info.def_count_not_including_drop();
                    if dest_def_count == 0 {
                        debug!("  Can't copy-propagate local: dest {:?} undefined",
                               dest_local);
                        continue
                    }
                    if dest_def_count > 1 {
                        debug!("  Can't copy-propagate local: dest {:?} defined {} times",
                               dest_local,
                               dest_use_info.def_count());
                        continue
                    }
                    if dest_use_info.use_count() == 0 {
                        debug!("  Can't copy-propagate local: dest {:?} unused",
                               dest_local);
                        continue
                    }
                    // Conservatively gives up if the dest is an argument,
                    // because there may be uses of the original argument value.
                    if mir.local_kind(dest_local) == LocalKind::Arg {
                        debug!("  Can't copy-propagate local: dest {:?} (argument)",
                            dest_local);
                        continue;
                    }
                    let dest_lvalue_def = dest_use_info.defs_not_including_drop().next().unwrap();
                    location = dest_lvalue_def.location;

                    let basic_block = &mir[location.block];
                    let statement_index = location.statement_index;
                    let statement = match basic_block.statements.get(statement_index) {
                        Some(statement) => statement,
                        None => {
                            debug!("  Can't copy-propagate local: used in terminator");
                            continue
                        }
                    };

                    // That use of the source must be an assignment.
                    match statement.kind {
                        StatementKind::Assign(Lvalue::Local(local), Rvalue::Use(ref operand)) if
                                local == dest_local => {
                            let maybe_action = match *operand {
                                Operand::Copy(ref src_lvalue) |
                                Operand::Move(ref src_lvalue) => {
                                    Action::local_copy(&mir, &def_use_analysis, src_lvalue)
                                }
                                Operand::Constant(ref src_constant) => {
                                    Action::constant(src_constant)
                                }
                            };
                            match maybe_action {
                                Some(this_action) => action = this_action,
                                None => continue,
                            }
                        }
                        _ => {
                            debug!("  Can't copy-propagate local: source use is not an \
                                    assignment");
                            continue
                        }
                    }
                }

                changed = action.perform(mir, &def_use_analysis, dest_local, location) || changed;
                // FIXME(pcwalton): Update the use-def chains to delete the instructions instead of
                // regenerating the chains.
                break
            }
            if !changed {
                break
            }
        }
    }
}

fn eliminate_self_assignments<'tcx>(
    mir: &mut Mir<'tcx>,
    def_use_analysis: &DefUseAnalysis<'tcx>,
) -> bool {
    let mut changed = false;

    for dest_local in mir.local_decls.indices() {
        let dest_use_info = def_use_analysis.local_info(dest_local);

        for def in dest_use_info.defs_not_including_drop() {
            let location = def.location;
            if let Some(stmt) = mir[location.block].statements.get(location.statement_index) {
                match stmt.kind {
                    StatementKind::Assign(
                        Lvalue::Local(local),
                        Rvalue::Use(Operand::Copy(Lvalue::Local(src_local))),
                    ) |
                    StatementKind::Assign(
                        Lvalue::Local(local),
                        Rvalue::Use(Operand::Move(Lvalue::Local(src_local))),
                    ) if local == dest_local && dest_local == src_local => {}
                    _ => {
                        continue;
                    }
                }
            } else {
                continue;
            }
            debug!("Deleting a self-assignment for {:?}", dest_local);
            mir.make_statement_nop(location);
            changed = true;
        }
    }

    changed
}

enum Action<'tcx> {
    PropagateLocalCopy(Local),
    PropagateConstant(Constant<'tcx>),
}

impl<'tcx> Action<'tcx> {
    fn local_copy(mir: &Mir<'tcx>, def_use_analysis: &DefUseAnalysis, src_lvalue: &Lvalue<'tcx>)
                  -> Option<Action<'tcx>> {
        // The source must be a local.
        let src_local = if let Lvalue::Local(local) = *src_lvalue {
            local
        } else {
            debug!("  Can't copy-propagate local: source is not a local");
            return None;
        };

        // We're trying to copy propagate a local.
        // There must be exactly one use of the source used in a statement (not in a terminator).
        let src_use_info = def_use_analysis.local_info(src_local);
        let src_use_count = src_use_info.use_count();
        if src_use_count == 0 {
            debug!("  Can't copy-propagate local: no uses");
            return None
        }
        if src_use_count != 1 {
            debug!("  Can't copy-propagate local: {} uses", src_use_info.use_count());
            return None
        }

        // Verify that the source doesn't change in between. This is done conservatively for now,
        // by ensuring that the source has exactly one mutation. The goal is to prevent things
        // like:
        //
        //     DEST = SRC;
        //     SRC = X;
        //     USE(DEST);
        //
        // From being misoptimized into:
        //
        //     SRC = X;
        //     USE(SRC);
        let src_def_count = src_use_info.def_count_not_including_drop();
        // allow function arguments to be propagated
        if src_def_count > 1 ||
            (src_def_count == 0 && mir.local_kind(src_local) != LocalKind::Arg) {
            debug!("  Can't copy-propagate local: {} defs of src",
                   src_use_info.def_count_not_including_drop());
            return None
        }

        Some(Action::PropagateLocalCopy(src_local))
    }

    fn constant(src_constant: &Constant<'tcx>) -> Option<Action<'tcx>> {
        Some(Action::PropagateConstant((*src_constant).clone()))
    }

    fn perform(self,
               mir: &mut Mir<'tcx>,
               def_use_analysis: &DefUseAnalysis<'tcx>,
               dest_local: Local,
               location: Location)
               -> bool {
        match self {
            Action::PropagateLocalCopy(src_local) => {
                // Eliminate the destination and the assignment.
                //
                // First, remove all markers.
                //
                // FIXME(pcwalton): Don't do this. Merge live ranges instead.
                debug!("  Replacing all uses of {:?} with {:?} (local)",
                       dest_local,
                       src_local);
                for lvalue_use in &def_use_analysis.local_info(dest_local).defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }
                for lvalue_use in &def_use_analysis.local_info(src_local).defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }

                // Replace all uses of the destination local with the source local.
                def_use_analysis.replace_all_defs_and_uses_with(dest_local, mir, src_local);

                // Finally, zap the now-useless assignment instruction.
                debug!("  Deleting assignment");
                mir.make_statement_nop(location);

                true
            }
            Action::PropagateConstant(src_constant) => {
                // First, remove all markers.
                //
                // FIXME(pcwalton): Don't do this. Merge live ranges instead.
                debug!("  Replacing all uses of {:?} with {:?} (constant)",
                       dest_local,
                       src_constant);
                let dest_local_info = def_use_analysis.local_info(dest_local);
                for lvalue_use in &dest_local_info.defs_and_uses {
                    if lvalue_use.context.is_storage_marker() {
                        mir.make_statement_nop(lvalue_use.location)
                    }
                }

                // Replace all uses of the destination local with the constant.
                let mut visitor = ConstantPropagationVisitor::new(dest_local,
                                                                  src_constant);
                for dest_lvalue_use in &dest_local_info.defs_and_uses {
                    visitor.visit_location(mir, dest_lvalue_use.location)
                }

                // Zap the assignment instruction if we eliminated all the uses. We won't have been
                // able to do that if the destination was used in a projection, because projections
                // must have lvalues on their LHS.
                let use_count = dest_local_info.use_count();
                if visitor.uses_replaced == use_count {
                    debug!("  {} of {} use(s) replaced; deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    mir.make_statement_nop(location);
                    true
                } else if visitor.uses_replaced == 0 {
                    debug!("  No uses replaced; not deleting assignment");
                    false
                } else {
                    debug!("  {} of {} use(s) replaced; not deleting assignment",
                           visitor.uses_replaced,
                           use_count);
                    true
                }
            }
        }
    }
}

struct ConstantPropagationVisitor<'tcx> {
    dest_local: Local,
    constant: Constant<'tcx>,
    uses_replaced: usize,
}

impl<'tcx> ConstantPropagationVisitor<'tcx> {
    fn new(dest_local: Local, constant: Constant<'tcx>)
           -> ConstantPropagationVisitor<'tcx> {
        ConstantPropagationVisitor {
            dest_local,
            constant,
            uses_replaced: 0,
        }
    }
}

impl<'tcx> MutVisitor<'tcx> for ConstantPropagationVisitor<'tcx> {
    fn visit_operand(&mut self, operand: &mut Operand<'tcx>, location: Location) {
        self.super_operand(operand, location);

        match *operand {
            Operand::Copy(Lvalue::Local(local)) |
            Operand::Move(Lvalue::Local(local)) if local == self.dest_local => {}
            _ => return,
        }

        *operand = Operand::Constant(box self.constant.clone());
        self.uses_replaced += 1
    }
}
