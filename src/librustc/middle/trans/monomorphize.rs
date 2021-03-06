// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use back::link::exported_name;
use driver::session;
use lib::llvm::ValueRef;
use middle::trans::base::{set_llvm_fn_attrs, set_inline_hint};
use middle::trans::base::{trans_enum_variant, push_ctxt, get_item_val};
use middle::trans::base::{trans_fn, decl_internal_rust_fn};
use middle::trans::base;
use middle::trans::common::*;
use middle::trans::intrinsic;
use middle::ty;
use middle::typeck;
use util::ppaux::Repr;

use syntax::abi;
use syntax::ast;
use syntax::ast_map;
use syntax::ast_util::local_def;
use std::hash::{sip, Hash};

pub fn monomorphic_fn(ccx: &CrateContext,
                      fn_id: ast::DefId,
                      real_substs: &ty::substs,
                      vtables: Option<typeck::vtable_res>,
                      self_vtables: Option<typeck::vtable_param_res>,
                      ref_id: Option<ast::NodeId>)
    -> (ValueRef, bool) {
    debug!("monomorphic_fn(\
            fn_id={}, \
            real_substs={}, \
            vtables={}, \
            self_vtable={}, \
            ref_id={:?})",
           fn_id.repr(ccx.tcx()),
           real_substs.repr(ccx.tcx()),
           vtables.repr(ccx.tcx()),
           self_vtables.repr(ccx.tcx()),
           ref_id);

    assert!(real_substs.tps.iter().all(|t| {
        !ty::type_needs_infer(*t) && !ty::type_has_params(*t)
    }));

    let _icx = push_ctxt("monomorphic_fn");

    let substs_iter = real_substs.self_ty.iter().chain(real_substs.tps.iter());
    let param_ids: Vec<MonoParamId> = match vtables {
        Some(ref vts) => {
            debug!("make_mono_id vtables={} psubsts={}",
                   vts.repr(ccx.tcx()), real_substs.tps.repr(ccx.tcx()));
            let vts_iter = self_vtables.iter().chain(vts.iter());
            vts_iter.zip(substs_iter).map(|(vtable, subst)| MonoParamId {
                subst: *subst,
                // Do we really need the vtables to be hashed? Isn't the type enough?
                vtables: vtable.iter().map(|vt| make_vtable_id(ccx, vt)).collect()
            }).collect()
        }
        None => substs_iter.map(|subst| MonoParamId {
            subst: *subst,
            vtables: Vec::new()
        }).collect()
    };

    let hash_id = MonoId {
        def: fn_id,
        params: param_ids
    };

    match ccx.monomorphized.borrow().find(&hash_id) {
        Some(&val) => {
            debug!("leaving monomorphic fn {}",
            ty::item_path_str(ccx.tcx(), fn_id));
            return (val, false);
        }
        None => ()
    }

    let psubsts = param_substs {
        substs: (*real_substs).clone(),
        vtables: vtables,
        self_vtables: self_vtables
    };

    debug!("monomorphic_fn(\
            fn_id={}, \
            psubsts={}, \
            hash_id={:?})",
           fn_id.repr(ccx.tcx()),
           psubsts.repr(ccx.tcx()),
           hash_id);

    let tpt = ty::lookup_item_type(ccx.tcx(), fn_id);
    let llitem_ty = tpt.ty;

    // We need to do special handling of the substitutions if we are
    // calling a static provided method. This is sort of unfortunate.
    let mut is_static_provided = None;

    let map_node = session::expect(
        ccx.sess(),
        ccx.tcx.map.find(fn_id.node),
        || {
            format!("while monomorphizing {:?}, couldn't find it in \
                     the item map (may have attempted to monomorphize \
                     an item defined in a different crate?)",
                    fn_id)
        });

    match map_node {
        ast_map::NodeForeignItem(_) => {
            if ccx.tcx.map.get_foreign_abi(fn_id.node) != abi::RustIntrinsic {
                // Foreign externs don't have to be monomorphized.
                return (get_item_val(ccx, fn_id.node), true);
            }
        }
        ast_map::NodeTraitMethod(method) => {
            match *method {
                ast::Provided(m) => {
                    // If this is a static provided method, indicate that
                    // and stash the number of params on the method.
                    if m.explicit_self.node == ast::SelfStatic {
                        is_static_provided = Some(m.generics.ty_params.len());
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }

    debug!("monomorphic_fn about to subst into {}", llitem_ty.repr(ccx.tcx()));
    let mono_ty = match is_static_provided {
        None => ty::subst(ccx.tcx(), real_substs, llitem_ty),
        Some(num_method_ty_params) => {
            // Static default methods are a little unfortunate, in
            // that the "internal" and "external" type of them differ.
            // Internally, the method body can refer to Self, but the
            // externally visible type of the method has a type param
            // inserted in between the trait type params and the
            // method type params. The substs that we are given are
            // the proper substs *internally* to the method body, so
            // we have to use those when compiling it.
            //
            // In order to get the proper substitution to use on the
            // type of the method, we pull apart the substitution and
            // stick a substitution for the self type in.
            // This is a bit unfortunate.

            let idx = real_substs.tps.len() - num_method_ty_params;
            let mut tps = Vec::new();
            tps.push_all(real_substs.tps.slice(0, idx));
            tps.push(real_substs.self_ty.unwrap());
            tps.push_all(real_substs.tps.tailn(idx));

            let substs = ty::substs { regions: ty::ErasedRegions,
                                      self_ty: None,
                                      tps: tps };

            debug!("static default: changed substitution to {}",
                   substs.repr(ccx.tcx()));

            ty::subst(ccx.tcx(), &substs, llitem_ty)
        }
    };

    ccx.stats.n_monos.set(ccx.stats.n_monos.get() + 1);

    let depth;
    {
        let mut monomorphizing = ccx.monomorphizing.borrow_mut();
        depth = match monomorphizing.find(&fn_id) {
            Some(&d) => d, None => 0
        };

        // Random cut-off -- code that needs to instantiate the same function
        // recursively more than thirty times can probably safely be assumed
        // to be causing an infinite expansion.
        if depth > ccx.sess().recursion_limit.get() {
            ccx.sess().span_fatal(ccx.tcx.map.span(fn_id.node),
                "reached the recursion limit during monomorphization");
        }

        monomorphizing.insert(fn_id, depth + 1);
    }

    let s = ccx.tcx.map.with_path(fn_id.node, |path| {
        let mut state = sip::SipState::new();
        hash_id.hash(&mut state);
        mono_ty.hash(&mut state);

        exported_name(path,
                      format!("h{}", state.result()).as_slice(),
                      ccx.link_meta.crateid.version_or_default())
    });
    debug!("monomorphize_fn mangled to {}", s);

    // This shouldn't need to option dance.
    let mut hash_id = Some(hash_id);
    let mk_lldecl = || {
        let lldecl = decl_internal_rust_fn(ccx, mono_ty, s.as_slice());
        ccx.monomorphized.borrow_mut().insert(hash_id.take_unwrap(), lldecl);
        lldecl
    };

    let lldecl = match map_node {
        ast_map::NodeItem(i) => {
            match *i {
              ast::Item {
                  node: ast::ItemFn(decl, _, _, _, body),
                  ..
              } => {
                  let d = mk_lldecl();
                  set_llvm_fn_attrs(i.attrs.as_slice(), d);
                  trans_fn(ccx, decl, body, d, Some(&psubsts), fn_id.node, []);
                  d
              }
              _ => {
                ccx.sess().bug("Can't monomorphize this kind of item")
              }
            }
        }
        ast_map::NodeForeignItem(i) => {
            let simple = intrinsic::get_simple_intrinsic(ccx, i);
            match simple {
                Some(decl) => decl,
                None => {
                    let d = mk_lldecl();
                    intrinsic::trans_intrinsic(ccx, d, i, &psubsts, ref_id);
                    d
                }
            }
        }
        ast_map::NodeVariant(v) => {
            let parent = ccx.tcx.map.get_parent(fn_id.node);
            let tvs = ty::enum_variants(ccx.tcx(), local_def(parent));
            let this_tv = tvs.iter().find(|tv| { tv.id.node == fn_id.node}).unwrap();
            let d = mk_lldecl();
            set_inline_hint(d);
            match v.node.kind {
                ast::TupleVariantKind(ref args) => {
                    trans_enum_variant(ccx,
                                       parent,
                                       v,
                                       args.as_slice(),
                                       this_tv.disr_val,
                                       Some(&psubsts),
                                       d);
                }
                ast::StructVariantKind(_) =>
                    ccx.sess().bug("can't monomorphize struct variants"),
            }
            d
        }
        ast_map::NodeMethod(mth) => {
            let d = mk_lldecl();
            set_llvm_fn_attrs(mth.attrs.as_slice(), d);
            trans_fn(ccx, mth.decl, mth.body, d, Some(&psubsts), mth.id, []);
            d
        }
        ast_map::NodeTraitMethod(method) => {
            match *method {
                ast::Provided(mth) => {
                    let d = mk_lldecl();
                    set_llvm_fn_attrs(mth.attrs.as_slice(), d);
                    trans_fn(ccx, mth.decl, mth.body, d, Some(&psubsts), mth.id, []);
                    d
                }
                _ => {
                    ccx.sess().bug(format!("can't monomorphize a {:?}",
                                           map_node).as_slice())
                }
            }
        }
        ast_map::NodeStructCtor(struct_def) => {
            let d = mk_lldecl();
            set_inline_hint(d);
            base::trans_tuple_struct(ccx,
                                     struct_def.fields.as_slice(),
                                     struct_def.ctor_id.expect("ast-mapped tuple struct \
                                                                didn't have a ctor id"),
                                     Some(&psubsts),
                                     d);
            d
        }

        // Ugh -- but this ensures any new variants won't be forgotten
        ast_map::NodeLifetime(..) |
        ast_map::NodeExpr(..) |
        ast_map::NodeStmt(..) |
        ast_map::NodeArg(..) |
        ast_map::NodeBlock(..) |
        ast_map::NodePat(..) |
        ast_map::NodeLocal(..) => {
            ccx.sess().bug(format!("can't monomorphize a {:?}",
                                   map_node).as_slice())
        }
    };

    ccx.monomorphizing.borrow_mut().insert(fn_id, depth);

    debug!("leaving monomorphic fn {}", ty::item_path_str(ccx.tcx(), fn_id));
    (lldecl, false)
}

// Used to identify cached monomorphized functions and vtables
#[deriving(Eq, TotalEq, Hash)]
pub struct MonoParamId {
    pub subst: ty::t,
    // Do we really need the vtables to be hashed? Isn't the type enough?
    pub vtables: Vec<MonoId>
}

#[deriving(Eq, TotalEq, Hash)]
pub struct MonoId {
    pub def: ast::DefId,
    pub params: Vec<MonoParamId>
}

pub fn make_vtable_id(ccx: &CrateContext,
                      origin: &typeck::vtable_origin)
                      -> MonoId {
    match origin {
        &typeck::vtable_static(impl_id, ref substs, ref sub_vtables) => {
            MonoId {
                def: impl_id,
                // FIXME(NDM) -- this is pretty bogus. It ignores self-type,
                // and vtables are not necessary, AND they are not guaranteed
                // to be same length as the number of TPS ANYHOW!
                params: sub_vtables.iter().zip(substs.tps.iter()).map(|(vtable, subst)| {
                    MonoParamId {
                        subst: *subst,
                        // Do we really need the vtables to be hashed? Isn't the type enough?
                        vtables: vtable.iter().map(|vt| make_vtable_id(ccx, vt)).collect()
                    }
                }).collect()
            }
        }

        // can't this be checked at the callee?
        _ => fail!("make_vtable_id needs vtable_static")
    }
}
