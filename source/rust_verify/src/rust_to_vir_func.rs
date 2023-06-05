use crate::attributes::{
    get_fuel, get_mode, get_publish, get_ret_mode, get_var_mode, get_verifier_attrs, VerifierAttrs,
};
use crate::context::{BodyCtxt, Context};
use crate::rust_to_vir_base::mk_visibility;
use crate::rust_to_vir_base::{
    check_generics_bounds_fun, def_id_to_vir_path, foreign_param_to_var, mid_ty_to_vir,
};
use crate::rust_to_vir_expr::{expr_to_vir, pat_to_mut_var, ExprModifier};
use crate::util::{err_span, unsupported_err_span};
use crate::{unsupported_err, unsupported_err_unless};
use rustc_ast::Attribute;
use rustc_hir::{
    def::Res, Body, BodyId, Crate, ExprKind, FnDecl, FnHeader, FnRetTy, FnSig, Generics, HirId,
    MaybeOwner, MutTy, Param, PrimTy, QPath, Ty, TyKind, Unsafety,
};
use rustc_middle::ty::Predicate;
use rustc_middle::ty::SubstsRef;
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::DefId;
use rustc_span::symbol::{Ident, Symbol};
use rustc_span::Span;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use vir::ast::{
    Fun, FunX, FunctionAttrsX, FunctionKind, FunctionX, GenericBoundX, KrateX, MaskSpec, Mode,
    ParamX, Typ, TypX, VirErr,
};
use vir::def::{RETURN_VALUE, VERUS_SPEC};

pub(crate) fn autospec_fun(path: &vir::ast::Path, method_name: String) -> vir::ast::Path {
    // turn a::b::c into a::b::method_name
    let mut pathx = (**path).clone();
    let mut segments = (*pathx.segments).clone();
    *segments.last_mut().expect("empty path") = Arc::new(method_name);
    pathx.segments = Arc::new(segments);
    Arc::new(pathx)
}

fn body_id_to_types<'tcx>(
    tcx: TyCtxt<'tcx>,
    id: &BodyId,
) -> &'tcx rustc_middle::ty::TypeckResults<'tcx> {
    let def = rustc_middle::ty::WithOptConstParam::unknown(id.hir_id.owner.def_id);
    tcx.typeck_opt_const_arg(def)
}

pub(crate) fn body_to_vir<'tcx>(
    ctxt: &Context<'tcx>,
    fun_id: DefId,
    id: &BodyId,
    body: &Body<'tcx>,
    mode: Mode,
    external_body: bool,
) -> Result<vir::ast::Expr, VirErr> {
    let types = body_id_to_types(ctxt.tcx, id);
    let bctx = BodyCtxt {
        ctxt: ctxt.clone(),
        types,
        fun_id,
        mode,
        external_body,
        in_ghost: mode != Mode::Exec,
    };
    expr_to_vir(&bctx, &body.value, ExprModifier::REGULAR)
}

fn check_fn_decl<'tcx>(
    span: Span,
    tcx: TyCtxt<'tcx>,
    decl: &'tcx FnDecl<'tcx>,
    attrs: &[Attribute],
    mode: Mode,
    output_ty: rustc_middle::ty::Ty<'tcx>,
) -> Result<Option<(Typ, Mode)>, VirErr> {
    let FnDecl { inputs: _, output, c_variadic, implicit_self, lifetime_elision_allowed: _ } = decl;
    unsupported_err_unless!(!c_variadic, span, "c_variadic functions");
    match implicit_self {
        rustc_hir::ImplicitSelfKind::None => {}
        rustc_hir::ImplicitSelfKind::Imm => {}
        rustc_hir::ImplicitSelfKind::ImmRef => {}
        rustc_hir::ImplicitSelfKind::MutRef => {}
        rustc_hir::ImplicitSelfKind::Mut => unsupported_err!(span, "mut self"),
    }
    match output {
        rustc_hir::FnRetTy::DefaultReturn(_) => Ok(None),
        // REVIEW: there's no attribute syntax on return types,
        // so we always return the default mode.
        // The current workaround is to return a struct if the default doesn't work.
        rustc_hir::FnRetTy::Return(_ty) => {
            let typ = mid_ty_to_vir(tcx, span, &output_ty, false)?;
            Ok(Some((typ, get_ret_mode(mode, attrs))))
        }
    }
}

pub(crate) fn find_body_krate<'tcx>(
    krate: &'tcx Crate<'tcx>,
    body_id: &BodyId,
) -> &'tcx Body<'tcx> {
    let owner = krate.owners[body_id.hir_id.owner.def_id];
    if let MaybeOwner::Owner(owner) = owner {
        if let Some(body) = owner.nodes.bodies.get(&body_id.hir_id.local_id) {
            return body;
        }
    }
    panic!("Body not found");
}

fn find_body<'tcx>(ctxt: &Context<'tcx>, body_id: &BodyId) -> &'tcx Body<'tcx> {
    find_body_krate(ctxt.krate, body_id)
}

fn check_new_strlit<'tcx>(ctx: &Context<'tcx>, sig: &'tcx FnSig<'tcx>) -> Result<(), VirErr> {
    let (decl, span) = match sig {
        FnSig { decl, span, .. } => (decl, span),
    };

    let sig_span = span;
    let expected_input_num = 1;

    if decl.inputs.len() != expected_input_num {
        return err_span(*sig_span, format!("Expected one argument to new_strlit"));
    }

    let (kind, span) = match &decl.inputs[0].kind {
        TyKind::Ref(_, MutTy { ty, mutbl: _ }) => (&ty.kind, ty.span),
        _ => {
            dbg!(&decl.inputs[0]);
            return err_span(decl.inputs[0].span, format!("expected a str"));
        }
    };

    let (res, span) = match kind {
        TyKind::Path(QPath::Resolved(_, path)) => (path.res, path.span),
        _ => return err_span(span, format!("expected str")),
    };

    if res != Res::PrimTy(PrimTy::Str) {
        return err_span(span, format!("expected a str"));
    }

    let (kind, span) = match decl.output {
        FnRetTy::Return(Ty { hir_id: _, kind, span }) => (kind, span),
        _ => return err_span(*sig_span, format!("expected a return type of StrSlice")),
    };

    let (res, span) = match kind {
        TyKind::Path(QPath::Resolved(_, path)) => (path.res, path.span),
        _ => return err_span(*span, format!("expected a StrSlice")),
    };

    let id = match res {
        Res::Def(_, id) => id,
        _ => return err_span(span, format!("")),
    };

    if !ctx.tcx.is_diagnostic_item(Symbol::intern("pervasive::string::StrSlice"), id) {
        return err_span(span, format!("expected a StrSlice"));
    }
    Ok(())
}

pub(crate) fn handle_external_fn<'tcx>(
    ctxt: &Context<'tcx>,
    id: DefId,
    visibility: vir::ast::Visibility,
    sig: &'tcx FnSig<'tcx>,
    // (impl generics, impl def_id)
    self_generics: Option<(&'tcx Generics, DefId)>,
    body_id: &CheckItemFnEither<&BodyId, &[Ident]>,
    mode: Mode,
    vattrs: &VerifierAttrs,
) -> Result<(vir::ast::Path, vir::ast::Visibility), VirErr> {
    // This function is the proxy, and we need to look up the actual path.

    if mode != Mode::Exec {
        return err_span(
            sig.span,
            format!("a function marked `external_fn_specification` cannot be marked `{mode:}`",),
        );
    }

    if vattrs.external {
        return err_span(
            sig.span,
            format!("a function cannot be marked both `external_fn_specification` and `external`",),
        );
    }
    if vattrs.external_body {
        return err_span(
            sig.span,
            format!(
                "a function cannot be marked both `external_fn_specification` and `external_body`",
            ),
        );
    }

    if self_generics.is_some() {
        return err_span(sig.span, "`external_fn_specification` attribute not supported here");
    }

    if let Some(impl_def_id) = ctxt.tcx.impl_of_method(id) {
        if ctxt.tcx.impl_trait_ref(impl_def_id).is_some() {
            return err_span(
                sig.span,
                "external_fn_specification not supported for trait functions",
            );
        }
    }

    if vattrs.autospec.is_some() {
        return err_span(
            sig.span,
            "`external_fn_specification` attribute not yet supported with `when_used_as_spec`",
        );
    }

    let body_id = match body_id {
        CheckItemFnEither::BodyId(body_id) => body_id,
        _ => {
            return err_span(
                sig.span,
                "external_fn_specification not supported for trait functions",
            );
        }
    };

    let body = find_body(ctxt, body_id);
    let external_id = get_external_def_id(ctxt.tcx, body_id, body, sig)?;
    let external_path = def_id_to_vir_path(ctxt.tcx, external_id);

    if ctxt.tcx.trait_of_item(external_id).is_some() {
        return err_span(sig.span, "external_fn_specification not supported for trait functions");
    }

    if external_path.krate == Some(Arc::new("builtin".to_string())) {
        return err_span(
            sig.span,
            "cannot apply `external_fn_specification` to Verus builtin functions",
        );
    }

    let ty1 = ctxt.tcx.type_of(id);
    let ty2 = ctxt.tcx.type_of(external_id);
    use rustc_middle::ty::FnDef;
    let (poly_sig1, poly_sig2, substs1) = match (ty1.kind(), ty2.kind()) {
        (FnDef(def_id1, substs1), FnDef(def_id2, substs2)) if substs1.len() == substs2.len() => {
            // Note we substitute `substs1` in both cases
            // This is to ensure that we are comparing the type signatures with
            // the same type params even if the user inputs different generic params
            // note: rustc 1.69.0 has replaced bound_fn_sig with just fn_sig I think
            let poly_sig1 = ctxt.tcx.bound_fn_sig(*def_id1).subst(ctxt.tcx, substs1);
            let poly_sig2 = ctxt.tcx.bound_fn_sig(*def_id2).subst(ctxt.tcx, substs1);
            (poly_sig1, poly_sig2, substs1)
        }
        _ => {
            return err_span(
                sig.span,
                format!(
                    "external_fn_specification requires function type signature to match exactly (got `{ty1:#?}` and `{ty2:#?}`)"
                ),
            );
        }
    };

    // Make sure names of (lifetime) binders don't influence equality check
    let sig1_anon = ctxt.tcx.anonymize_bound_vars(poly_sig1);
    let sig2_anon = ctxt.tcx.anonymize_bound_vars(poly_sig2);

    if sig1_anon != sig2_anon {
        return err_span(
            sig.span,
            format!(
                "external_fn_specification requires function type signature to match exactly (got `{ty1:#?}` and `{ty2:#?}`)"
            ),
        );
    }
    // trait bounds aren't part of the type signature - we have to check those separately
    if !predicates_match(ctxt.tcx, id, external_id, substs1) {
        return err_span(
            sig.span,
            format!(
                "external_fn_specification requires function type signature to match exactly (trait bound mismatch)"
            ),
        );
    }

    let owning_module_of_external_item =
        crate::rust_to_vir_base::def_id_to_vir_module(ctxt.tcx, external_id);
    let external_item_visibility = mk_visibility(
        ctxt,
        &Some(owning_module_of_external_item), // REVIEW should this ever be None? what's owning module None mean?
        external_id,
    );
    if !vir::ast_util::is_visible_to_opt(&visibility, &external_item_visibility.restricted_to) {
        return err_span(
            sig.span,
            "a function marked `external_fn_specification` must be visible to the function it provides a spec for",
        );
    }

    Ok((external_path, external_item_visibility))
}

pub enum CheckItemFnEither<A, B> {
    BodyId(A),
    ParamNames(B),
}

pub(crate) fn check_item_fn<'tcx>(
    ctxt: &Context<'tcx>,
    functions: &mut Vec<vir::ast::Function>,
    id: DefId,
    kind: FunctionKind,
    visibility: vir::ast::Visibility,
    attrs: &[Attribute],
    sig: &'tcx FnSig<'tcx>,
    // (impl generics, impl def_id)
    self_generics: Option<(&'tcx Generics, DefId)>,
    generics: &'tcx Generics,
    body_id: CheckItemFnEither<&BodyId, &[Ident]>,
) -> Result<Option<Fun>, VirErr> {
    let this_path = def_id_to_vir_path(ctxt.tcx, id);

    let is_verus_spec = this_path.segments.last().expect("segment.last").starts_with(VERUS_SPEC);
    let is_new_strlit =
        ctxt.tcx.is_diagnostic_item(Symbol::intern("pervasive::string::new_strlit"), id);

    let vattrs = get_verifier_attrs(attrs)?;
    let mode = get_mode(Mode::Exec, attrs);

    let (path, proxy, visibility) = if vattrs.external_fn_specification {
        if is_verus_spec {
            return err_span(
                sig.span,
                "`external_fn_specification` attribute not supported with VERUS_SPEC",
            );
        }

        if is_new_strlit {
            return err_span(
                sig.span,
                "`external_fn_specification` attribute not supported with new_strlit",
            );
        }

        let (external_path, external_item_visibility) =
            handle_external_fn(ctxt, id, visibility, sig, self_generics, &body_id, mode, &vattrs)?;

        let proxy = (*ctxt.spanned_new(sig.span, this_path)).clone();

        (external_path, Some(proxy), external_item_visibility)
    } else {
        // No proxy.
        (this_path, None, visibility)
    };

    let name = Arc::new(FunX { path: path.clone() });

    if vattrs.external {
        let mut erasure_info = ctxt.erasure_info.borrow_mut();
        erasure_info.external_functions.push(name);
        return Ok(None);
    }

    let self_typ_params = if let Some((cg, impl_def_id)) = self_generics {
        Some(check_generics_bounds_fun(ctxt.tcx, cg, impl_def_id)?)
    } else {
        None
    };

    let fn_sig = ctxt.tcx.fn_sig(id);
    // REVIEW: rustc docs refer to skip_binder as "dangerous"
    let fn_sig = fn_sig.skip_binder();
    let inputs = fn_sig.inputs();

    let ret_typ_mode = match sig {
        FnSig {
            header: FnHeader { unsafety, constness: _, asyncness: _, abi: _ },
            decl,
            span: _,
        } => {
            unsupported_err_unless!(*unsafety == Unsafety::Normal, sig.span, "unsafe");
            check_fn_decl(sig.span, ctxt.tcx, decl, attrs, mode, fn_sig.output())?
        }
    };

    if is_new_strlit {
        check_new_strlit(&ctxt, sig)?;
    }

    if is_new_strlit {
        let mut erasure_info = ctxt.erasure_info.borrow_mut();
        unsupported_err_unless!(
            vattrs.external_body,
            sig.span,
            "StrSlice::new must be external_body"
        );

        erasure_info.ignored_functions.push((id, sig.span.data()));
        erasure_info.external_functions.push(name);
        return Ok(None);
    }

    let sig_typ_bounds = check_generics_bounds_fun(ctxt.tcx, generics, id)?;
    let fuel = get_fuel(&vattrs);

    let (vir_body, header, params): (_, _, Vec<(String, Span, Option<HirId>)>) = match body_id {
        CheckItemFnEither::BodyId(body_id) => {
            let body = find_body(ctxt, body_id);
            let Body { params, value: _, generator_kind } = body;
            match generator_kind {
                None => {}
                _ => {
                    unsupported_err!(sig.span, "generator_kind", generator_kind);
                }
            }
            let mut ps = Vec::new();
            for Param { hir_id, pat, ty_span: _, span } in params.iter() {
                let (is_mut_var, name) = pat_to_mut_var(pat)?;
                // is_mut_var: means a parameter is like `mut x: X` (unsupported)
                // is_mut: means a parameter is like `x: &mut X` or `x: Tracked<&mut X>`
                if is_mut_var {
                    return err_span(
                        *span,
                        format!(
                            "Verus does not support `mut` arguments (try writing `let mut param = param;` in the body of the function)"
                        ),
                    );
                }
                ps.push((name, *span, Some(*hir_id)));
            }
            let mut vir_body = body_to_vir(ctxt, id, body_id, body, mode, vattrs.external_body)?;
            let header = vir::headers::read_header(&mut vir_body)?;
            (Some(vir_body), header, ps)
        }
        CheckItemFnEither::ParamNames(params) => {
            let params = params.iter().map(|p| (p.to_string(), p.span, None)).collect();
            let header = vir::headers::read_header_block(&mut vec![])?;
            (None, header, params)
        }
    };

    let mut vir_params: Vec<(vir::ast::Param, Option<Mode>)> = Vec::new();
    assert!(params.len() == inputs.len());
    for ((name, span, hir_id), input) in params.into_iter().zip(inputs.iter()) {
        let name = Arc::new(name);
        let param_mode = if let Some(hir_id) = hir_id {
            get_var_mode(mode, ctxt.tcx.hir().attrs(hir_id))
        } else {
            assert!(matches!(kind, FunctionKind::TraitMethodDecl { .. }));
            // This case is for a trait method declaration,
            // where the mode will later be overridden by the separate spec method anyway:
            Mode::Exec
        };
        let is_ref_mut = is_mut_ty(ctxt, *input);
        if is_ref_mut.is_some() && mode == Mode::Spec {
            return err_span(
                span,
                format!("&mut argument not allowed for #[verifier::spec] functions"),
            );
        }

        let typ =
            mid_ty_to_vir(ctxt.tcx, span, is_ref_mut.map(|(t, _)| t).unwrap_or(input), false)?;

        // is_mut: means a parameter is like `x: &mut X` or `x: Tracked<&mut X>`
        let is_mut = is_ref_mut.is_some();

        let vir_param = ctxt.spanned_new(
            span,
            ParamX { name, typ, mode: param_mode, is_mut, unwrapped_info: None },
        );
        vir_params.push((vir_param, is_ref_mut.map(|(_, m)| m).flatten()));
    }

    match (&kind, header.no_method_body, is_verus_spec, vir_body.is_some()) {
        (FunctionKind::TraitMethodDecl { .. }, false, false, false) => {}
        (FunctionKind::TraitMethodDecl { .. }, false, false, true) => {
            return err_span(sig.span, "trait default methods are not yet supported");
        }
        (FunctionKind::TraitMethodDecl { .. }, true, true, _) => {}
        (FunctionKind::TraitMethodDecl { .. }, false, true, _) => {
            return err_span(
                sig.span,
                "trait method declaration body must end with call to no_method_body()",
            );
        }
        (_, _, true, _) => {
            return err_span(
                sig.span,
                format!("{VERUS_SPEC} can only appear in trait method declarations"),
            );
        }
        (_, false, false, true) => {}
        (_, false, false, false) => {
            return err_span(sig.span, "function must have a body");
        }
        (_, true, _, _) => {
            return err_span(
                sig.span,
                "no_method_body can only appear in trait method declarations",
            );
        }
    }
    if mode == Mode::Spec && (header.require.len() + header.ensure.len()) > 0 {
        return err_span(sig.span, "spec functions cannot have requires/ensures");
    }
    if mode != Mode::Spec && header.recommend.len() > 0 {
        return err_span(sig.span, "non-spec functions cannot have recommends");
    }
    if mode != Mode::Exec && vattrs.external_fn_specification {
        return err_span(sig.span, "external_fn_specification should be 'exec'");
    }
    if header.ensure.len() > 0 {
        match (&header.ensure_id_typ, ret_typ_mode.as_ref()) {
            (None, None) => {}
            (None, Some(_)) => {
                return err_span(
                    sig.span,
                    "the return value must be named in a function with an ensures clause",
                );
            }
            (Some(_), None) => {
                return err_span(
                    sig.span,
                    "unexpected named return value for function with default return",
                );
            }
            (Some((_, typ)), Some((ret_typ, _))) => {
                if !vir::ast_util::types_equal(&typ, &ret_typ) {
                    return err_span(
                        sig.span,
                        format!(
                            "return type is {:?}, but ensures expects type {:?}",
                            &ret_typ, &typ
                        ),
                    );
                }
            }
        }
    }

    use vir::ast::UnwrapParameter;
    let mut all_param_names: Vec<vir::ast::Ident> = Vec::new();
    let mut all_param_name_set: HashSet<vir::ast::Ident> = HashSet::new();
    let mut unwrap_param_map: HashMap<vir::ast::Ident, UnwrapParameter> = HashMap::new();
    for unwrap in header.unwrap_parameters.iter() {
        all_param_names.push(unwrap.inner_name.clone());
        unwrap_param_map.insert(unwrap.outer_name.clone(), unwrap.clone());
    }
    for (param, unwrap_mut) in vir_params.iter_mut() {
        all_param_names.push(param.x.name.clone());
        if let Some(unwrap) = unwrap_param_map.get(&param.x.name) {
            if mode != Mode::Exec {
                return err_span(
                    sig.span,
                    "only exec functions can use Ghost(x) or Tracked(x) parameters",
                );
            }
            let mut paramx = param.x.clone();
            paramx.name = unwrap.inner_name.clone();
            paramx.unwrapped_info = Some((unwrap.mode, unwrap.outer_name.clone()));
            *param = vir::def::Spanned::new(param.span.clone(), paramx);
        } else if unwrap_mut.is_some() {
            return err_span(sig.span, format!("parameter {} must be unwrapped", &param.x.name));
        }
    }
    for name in all_param_names.iter() {
        if all_param_name_set.contains(name) {
            return err_span(sig.span, format!("duplicate parameter name {}", name));
        }
        all_param_name_set.insert(name.clone());
    }
    let params: vir::ast::Params = Arc::new(vir_params.into_iter().map(|(p, _)| p).collect());

    let (ret_name, ret_typ, ret_mode) = match (header.ensure_id_typ, ret_typ_mode) {
        (None, None) => {
            (Arc::new(RETURN_VALUE.to_string()), Arc::new(TypX::Tuple(Arc::new(vec![]))), mode)
        }
        (None, Some((typ, mode))) => (Arc::new(RETURN_VALUE.to_string()), typ, mode),
        (Some((x, _)), Some((typ, mode))) => (x, typ, mode),
        _ => panic!("internal error: ret_typ"),
    };
    let ret = ctxt.spanned_new(
        sig.span,
        ParamX {
            name: ret_name,
            typ: ret_typ,
            mode: ret_mode,
            is_mut: false,
            unwrapped_info: None,
        },
    );
    let typ_bounds = {
        let mut typ_bounds: Vec<(vir::ast::Ident, vir::ast::GenericBound)> = Vec::new();
        if let FunctionKind::TraitMethodDecl { .. } = kind {
            let bound = GenericBoundX::Traits(vec![]);
            typ_bounds.push((vir::def::trait_self_type_param(), Arc::new(bound)));
        }
        if let Some(self_typ_params) = self_typ_params {
            typ_bounds.append(&mut (*self_typ_params).clone());
        }
        typ_bounds.extend_from_slice(&sig_typ_bounds[..]);
        Arc::new(typ_bounds)
    };
    let publish = get_publish(&vattrs);
    let autospec = vattrs.autospec.map(|method_name| {
        let path = autospec_fun(&path, method_name.clone());
        Arc::new(FunX { path })
    });

    if vattrs.nonlinear && vattrs.spinoff_prover {
        return err_span(
            sig.span,
            "#[verifier(spinoff_prover)] is implied for assert by nonlinear_arith",
        );
    }
    let fattrs = FunctionAttrsX {
        uses_ghost_blocks: vattrs.verus_macro,
        inline: vattrs.inline,
        hidden: Arc::new(header.hidden),
        custom_req_err: vattrs.custom_req_err,
        no_auto_trigger: vattrs.no_auto_trigger,
        broadcast_forall: vattrs.broadcast_forall,
        bit_vector: vattrs.bit_vector,
        autospec,
        atomic: vattrs.atomic,
        integer_ring: vattrs.integer_ring,
        is_decrease_by: vattrs.decreases_by,
        check_recommends: vattrs.check_recommends,
        nonlinear: vattrs.nonlinear,
        spinoff_prover: vattrs.spinoff_prover,
        memoize: vattrs.memoize,
    };

    let mut recommend: Vec<vir::ast::Expr> = (*header.recommend).clone();
    if let Some(decrease_when) = &header.decrease_when {
        // Automatically add decrease_when to recommends
        recommend.push(decrease_when.clone());
    }

    // This function is marked 'private' at the source level to prevent the user from
    // calling it. But we translate things to point to it internally, so we need to
    // mark it non-private in order to avoid errors down the line.
    let mut visibility = visibility;
    if path == vir::def::exec_nonstatic_call_path(&ctxt.vstd_crate_name) {
        visibility.restricted_to = None;
    }

    let func = FunctionX {
        name: name.clone(),
        proxy,
        kind,
        visibility,
        mode,
        fuel,
        typ_bounds,
        params,
        ret,
        require: if mode == Mode::Spec { Arc::new(recommend) } else { header.require },
        ensure: header.ensure,
        decrease: header.decrease,
        decrease_when: header.decrease_when,
        decrease_by: header.decrease_by,
        broadcast_forall: None,
        mask_spec: header.invariant_mask,
        is_const: false,
        publish,
        attrs: Arc::new(fattrs),
        body: if vattrs.external_body || vattrs.external_fn_specification || header.no_method_body {
            None
        } else {
            vir_body
        },
        extra_dependencies: header.extra_dependencies,
    };

    let function = ctxt.spanned_new(sig.span, func);
    functions.push(function);
    if is_verus_spec { Ok(None) } else { Ok(Some(name)) }
}

// &mut T => Some(T, None)
// Ghost<&mut T> => Some(T, Some(Spec))
// Tracked<&mut T> => Some(T, Some(Proof))
// _ => None
fn is_mut_ty<'tcx>(
    ctxt: &Context<'tcx>,
    ty: rustc_middle::ty::Ty<'tcx>,
) -> Option<(&'tcx rustc_middle::ty::Ty<'tcx>, Option<Mode>)> {
    use rustc_middle::ty::*;
    match ty.kind() {
        TyKind::Ref(_, tys, rustc_ast::Mutability::Mut) => Some((tys, None)),
        TyKind::Adt(AdtDef(adt_def_data), args) => {
            let did = adt_def_data.did;
            let def_name = vir::ast_util::path_as_rust_name(&def_id_to_vir_path(ctxt.tcx, did));
            let is_ghost = def_name == "builtin::Ghost" && args.len() == 1;
            let is_tracked = def_name == "builtin::Tracked" && args.len() == 1;
            if is_ghost || is_tracked {
                if let subst::GenericArgKind::Type(t) = args[0].unpack() {
                    if let Some((inner, None)) = is_mut_ty(ctxt, t) {
                        let mode = if is_ghost { Mode::Spec } else { Mode::Proof };
                        return Some((inner, Some(mode)));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn predicates_match<'tcx>(
    tcx: TyCtxt<'tcx>,
    id1: rustc_span::def_id::DefId,
    id2: rustc_span::def_id::DefId,
    substs: SubstsRef<'tcx>,
) -> bool {
    let preds1 = all_predicates(tcx, id1, substs);
    let preds2 = all_predicates(tcx, id2, substs);
    if preds1.len() != preds2.len() {
        return false;
    }

    // Note: it might actually be impossible for the below check to fail?
    // Since we already passed Rust's typechecking, one of the predicate lists
    // has to be a subset of the other. But we just checked they're the same size.
    // So they have to be equal.
    //
    // Regardless, it makes sense to keep this as a sanity check.

    let preds1 = preds1.iter().map(|p| tcx.anonymize_bound_vars(p.kind()));
    let mut preds2: Vec<_> = preds2.iter().map(|p| tcx.anonymize_bound_vars(p.kind())).collect();

    for p1 in preds1 {
        let mut found = false;
        for i in 0..preds2.len() {
            if p1 == preds2[i] {
                preds2.remove(i);
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    return true;
}

fn all_predicates<'tcx>(
    tcx: TyCtxt<'tcx>,
    id: rustc_span::def_id::DefId,
    substs: SubstsRef<'tcx>,
) -> Vec<Predicate<'tcx>> {
    let preds = tcx.predicates_of(id);
    let preds = preds.instantiate(tcx, substs);
    preds.predicates
}

pub(crate) fn get_external_def_id<'tcx>(
    tcx: TyCtxt<'tcx>,
    body_id: &BodyId,
    body: &Body<'tcx>,
    sig: &'tcx FnSig<'tcx>,
) -> Result<rustc_span::def_id::DefId, VirErr> {
    let err = || {
        err_span(
            sig.span,
            format!("external_fn_specification encoding error: body should end in call expression"),
        )
    };

    // Get the 'body' of this function (skipping over header if necessary)
    let expr = match &body.value.kind {
        ExprKind::Block(block_body, _) => match &block_body.expr {
            Some(body_value) => body_value,
            None => {
                return err();
            }
        },
        _ => &body.value,
    };

    match &expr.kind {
        ExprKind::Call(fun, _args) => match &fun.kind {
            ExprKind::Path(qpath) => {
                let types = body_id_to_types(tcx, body_id);
                let def = types.qpath_res(&qpath, fun.hir_id);
                match def {
                    rustc_hir::def::Res::Def(_, def_id) => {
                        // We don't need to check the args match or anything,
                        // the type signature check done by the caller is sufficient.
                        Ok(def_id)
                    }
                    _ => {
                        return err();
                    }
                }
            }
            _ => {
                return err();
            }
        },
        ExprKind::MethodCall(_name_and_generics, _receiver, _other_args, _fn_span) => {
            let types = body_id_to_types(tcx, body_id);
            let def_id =
                types.type_dependent_def_id(expr.hir_id).expect("def id of the method definition");
            Ok(def_id)
        }
        _ => {
            return err();
        }
    }
}

pub(crate) fn check_item_const<'tcx>(
    ctxt: &Context<'tcx>,
    vir: &mut KrateX,
    span: Span,
    id: DefId,
    visibility: vir::ast::Visibility,
    attrs: &[Attribute],
    typ: &Typ,
    body_id: &BodyId,
) -> Result<(), VirErr> {
    let path = def_id_to_vir_path(ctxt.tcx, id);
    let name = Arc::new(FunX { path });
    let mode = get_mode(Mode::Exec, attrs);
    let vattrs = get_verifier_attrs(attrs)?;

    if vattrs.external_fn_specification {
        return err_span(span, "`external_fn_specification` attribute not yet supported for const");
    }

    let fuel = get_fuel(&vattrs);
    if vattrs.external {
        let mut erasure_info = ctxt.erasure_info.borrow_mut();
        erasure_info.external_functions.push(name);
        return Ok(());
    }
    let body = find_body(ctxt, body_id);
    let vir_body = body_to_vir(ctxt, id, body_id, body, mode, vattrs.external_body)?;

    let ret_name = Arc::new(RETURN_VALUE.to_string());
    let ret = ctxt.spanned_new(
        span,
        ParamX { name: ret_name, typ: typ.clone(), mode, is_mut: false, unwrapped_info: None },
    );
    let func = FunctionX {
        name,
        proxy: None,
        kind: FunctionKind::Static,
        visibility,
        mode: Mode::Spec, // the function has mode spec; the mode attribute goes into ret.x.mode
        fuel,
        typ_bounds: Arc::new(vec![]),
        params: Arc::new(vec![]),
        ret,
        require: Arc::new(vec![]),
        ensure: Arc::new(vec![]),
        decrease: Arc::new(vec![]),
        decrease_when: None,
        decrease_by: None,
        broadcast_forall: None,
        mask_spec: MaskSpec::NoSpec,
        is_const: true,
        publish: get_publish(&vattrs),
        attrs: Default::default(),
        body: if vattrs.external_body { None } else { Some(vir_body) },
        extra_dependencies: vec![],
    };
    let function = ctxt.spanned_new(span, func);
    vir.functions.push(function);
    Ok(())
}

pub(crate) fn check_foreign_item_fn<'tcx>(
    ctxt: &Context<'tcx>,
    vir: &mut KrateX,
    id: DefId,
    span: Span,
    visibility: vir::ast::Visibility,
    attrs: &[Attribute],
    decl: &'tcx FnDecl<'tcx>,
    idents: &[Ident],
    generics: &'tcx Generics,
) -> Result<(), VirErr> {
    let mode = get_mode(Mode::Exec, attrs);

    let fn_sig = ctxt.tcx.fn_sig(id);
    // REVIEW: rustc docs refer to skip_binder as "dangerous"
    let fn_sig = fn_sig.skip_binder();
    let inputs = fn_sig.inputs();

    let ret_typ_mode = check_fn_decl(span, ctxt.tcx, decl, attrs, mode, fn_sig.output())?;
    let typ_bounds = check_generics_bounds_fun(ctxt.tcx, generics, id)?;
    let vattrs = get_verifier_attrs(attrs)?;
    let fuel = get_fuel(&vattrs);
    let mut vir_params: Vec<vir::ast::Param> = Vec::new();

    if vattrs.external_fn_specification {
        return err_span(
            span,
            "`external_fn_specification` attribute not supported on foreign items",
        );
    }

    assert!(idents.len() == inputs.len());
    for (param, input) in idents.iter().zip(inputs.iter()) {
        let name = Arc::new(foreign_param_to_var(param));
        let is_mut = is_mut_ty(ctxt, *input);
        let typ =
            mid_ty_to_vir(ctxt.tcx, param.span, is_mut.map(|(t, _)| t).unwrap_or(input), false)?;
        // REVIEW: the parameters don't have attributes, so we use the overall mode
        let vir_param = ctxt.spanned_new(
            param.span,
            ParamX { name, typ, mode, is_mut: is_mut.is_some(), unwrapped_info: None },
        );
        vir_params.push(vir_param);
    }
    let path = def_id_to_vir_path(ctxt.tcx, id);
    let name = Arc::new(FunX { path });
    let params = Arc::new(vir_params);
    let (ret_typ, ret_mode) = match ret_typ_mode {
        None => (Arc::new(TypX::Tuple(Arc::new(vec![]))), mode),
        Some((typ, mode)) => (typ, mode),
    };
    let ret_param = ParamX {
        name: Arc::new(RETURN_VALUE.to_string()),
        typ: ret_typ,
        mode: ret_mode,
        is_mut: false,
        unwrapped_info: None,
    };
    let ret = ctxt.spanned_new(span, ret_param);
    let func = FunctionX {
        name,
        proxy: None,
        kind: FunctionKind::Static,
        visibility,
        fuel,
        mode,
        typ_bounds,
        params,
        ret,
        require: Arc::new(vec![]),
        ensure: Arc::new(vec![]),
        decrease: Arc::new(vec![]),
        decrease_when: None,
        decrease_by: None,
        broadcast_forall: None,
        mask_spec: MaskSpec::NoSpec,
        is_const: false,
        publish: None,
        attrs: Default::default(),
        body: None,
        extra_dependencies: vec![],
    };
    let function = ctxt.spanned_new(span, func);
    vir.functions.push(function);
    Ok(())
}
