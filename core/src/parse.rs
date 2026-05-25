use crate::diagnostic::{Diagnostic, OORVError};
use num::rational::Rational64 as Rational;
use num::{BigInt, BigRational, ToPrimitive};
use pest::iterators::{Pair, Pairs};
use pest::pratt_parser::{Assoc, Op, PrattParser};
use pest::Parser;
use pest_derive::Parser;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::OnceLock;

use num::traits::Pow;
use std::str::FromStr;

pub use super::ast::*;

#[derive(Parser)]
#[grammar = "oorv.pest"]

struct OORVRule;

static PRATT_PARSER_ONCE: OnceLock<PrattParser<Rule>> = OnceLock::new();

fn pratt_parser() -> &'static PrattParser<Rule> {
    PRATT_PARSER_ONCE.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::OpImplies, Assoc::Right))
            .op(Op::infix(Rule::OpOr, Assoc::Left))
            .op(Op::infix(Rule::OpAnd, Assoc::Left))
            .op(Op::infix(Rule::OpBitOr, Assoc::Left))
            .op(Op::infix(Rule::OpBitXor, Assoc::Left))
            .op(Op::infix(Rule::OpBitAnd, Assoc::Left))
            .op(Op::infix(Rule::CmpEq, Assoc::Left) | Op::infix(Rule::CmpNe, Assoc::Left))
            .op(Op::infix(Rule::CmpLt, Assoc::Left)
                | Op::infix(Rule::CmpLe, Assoc::Left)
                | Op::infix(Rule::CmpGt, Assoc::Left)
                | Op::infix(Rule::CmpGe, Assoc::Left))
            .op(Op::infix(Rule::OpShl, Assoc::Left) | Op::infix(Rule::OpShr, Assoc::Left))
            .op(Op::infix(Rule::OpAdd, Assoc::Left) | Op::infix(Rule::OpSub, Assoc::Left))
            .op(Op::infix(Rule::OpMul, Assoc::Left)
                | Op::infix(Rule::OpDiv, Assoc::Left)
                | Op::infix(Rule::OpRem, Assoc::Left))
            .op(Op::infix(Rule::OpPow, Assoc::Right))
            .op(Op::infix(Rule::OpDot, Assoc::Left))
            .op(Op::infix(Rule::LBracket, Assoc::Left))
    })
}

#[derive(Debug, Clone)]
pub struct OORVSpecParser {
    ast: OORVAst,
    spec: String,
}

impl OORVSpecParser {
    /// Create a new parser instance.
    pub fn new(spec: String) -> Self {
        OORVSpecParser {
            ast: OORVAst::default(),
            spec,
        }
    }

    pub fn parse_for_ast(spec: String, source: String) -> Result<OORVAst, OORVError> {
        // Step 0: create parser instance.
        let parser = OORVSpecParser::new(spec.clone());

        // Set source context early so all diagnostics (including parse errors) can include
        // source annotations. Cleared only on error paths; success path keeps it alive
        // for the subsequent analysis pipeline.
        crate::diagnostic::set_source_context(&source, &spec);

        // Step 1: pre-process the source.
        // 1.1 include
        let pairs_for_include = OORVRule::parse(Rule::Spec, &spec).map_err(|e| {
            let d = Self::format_pest_error(e, &source, &spec);
            OORVError::from(d)
        })?;
        let mut seen_includes: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let include_base_dir = std::path::Path::new(&source).parent();
        let preprocessed_include_spec = match preprocess_include(
            pairs_for_include,
            &spec,
            include_base_dir,
            &mut seen_includes,
        ) {
            Ok(s) => s,
            Err(e) => return Err(e),
        };

        // 1.2 format
        let preprocessed_include_spec1 =
            match Self::preprocess_format_macros(&preprocessed_include_spec) {
                Ok(s) => s,
                Err(e) => return Err(e),
            };

        // Step 1.3: expand quantifier syntax sugar.
        let pairs_for_quant =
            OORVRule::parse(Rule::Spec, &preprocessed_include_spec1).map_err(|e| {
                let d = Self::format_pest_error(e, &source, &preprocessed_include_spec1);
                OORVError::from(d)
            })?;
        let preprocessed_quant_spec =
            match preprocess_quantifiers(pairs_for_quant, &preprocessed_include_spec1) {
                Ok(s) => s,
                Err(e) => return Err(e),
            };

        // Step 1.4: collect let-binding names and rewrite uses to name(id).
        let preprocessed_spec = match Self::preprocess_let_idents(&preprocessed_quant_spec) {
            Ok(s) => s,
            Err(e) => return Err(e),
        };

        // Step 2: parse the pre-processed source into an AST.
        // Update source context to the preprocessed spec for accurate span positions
        crate::diagnostic::set_source_context(&source, &preprocessed_spec);
        let mut ast = match parser.parse_spec(Some(&preprocessed_spec)) {
            Ok(a) => a,
            Err(e) => {
                crate::diagnostic::clear_source_context();
                return Err(e);
            }
        };

        // Step 3: merge parent class signals into each subclass.
        Self::merge_base_class_signals(&mut ast)?;

        // Step 4: resolve all class types to primitive types.
        Self::convert_class_types_to_basic(&mut ast)?;

        // Step 5: qualify signal names with their module/class prefix.
        Self::qualify_signal_names(&mut ast);

        // Step 6: validate that member type classes are declared in the used modules.
        Self::check_member_types_and_uses(&mut ast)?;

        // Step 6.1: substitute member references with ty_name in constrain expressions.
        Self::rewrite_member_refs(&mut ast);

        // Step 7: hoist signals of member-referenced classes to ast.signals.
        use std::collections::HashSet;
        let mut member_types: HashSet<String> = HashSet::new();
        for m in ast.members.iter() {
            // ty_name may be `Class` or `Module::Class` — take last segment
            let short = m
                .ty_name
                .name
                .split("::")
                .last()
                .unwrap_or(&m.ty_name.name)
                .to_string();
            member_types.insert(short);
        }

        let mut all_signals: Vec<Rc<crate::ast::Signal>> = Vec::new();
        for c in ast.classes.iter() {
            if !member_types.contains(&c.name.name) {
                continue;
            }
            for s in c.signals.iter() {
                all_signals.push(s.clone());
            }
        }
        ast.signals.extend(all_signals);

        // Step 8: generate constraints for each signal (delegated to helper).
        Self::add_signal_constrains(&mut ast);
        Ok(ast)
    }

    fn format_pest_error(e: pest::error::Error<Rule>, _source: &str, spec: &str) -> Diagnostic {
        let binding = e.to_string();
        let summary = binding.lines().next().unwrap_or("parse error");
        // Compute the byte offset of the error position for span labeling
        let (start, end) = match e.location {
            pest::error::InputLocation::Pos(p) => (p, (p + 1).min(spec.len())),
            pest::error::InputLocation::Span((s, e)) => (s, e),
        };
        let msg = format!("parse failed: {}", summary);
        Diagnostic::error(&msg)
            .add_span_range(start, end, Some("error here"), true)
            .try_attach_source()
    }

    /// Parse the given spec (or use internal input) into an AST.
    pub fn parse_spec(mut self, input_override: Option<&str>) -> Result<OORVAst, OORVError> {
        let spec_owned: String = match input_override {
            Some(s) => s.to_string(),
            None => self.spec.clone(),
        };
        let spec_str = spec_owned.as_str();
        let mut pairs = OORVRule::parse(Rule::Spec, spec_str).map_err(|e| {
            let d = OORVSpecParser::format_pest_error(e, "<input>", spec_str);
            OORVError::from(d)
        })?;
        // Track occurrences of WorldDecl: only one is allowed
        let mut world_count: usize = 0;
        let mut first_error: Option<OORVError> = None;
        let mut use_set: BTreeSet<String> = BTreeSet::new();
        let spec_pair = pairs.next().unwrap();
        for pair in spec_pair.into_inner() {
            match pair.as_rule() {
                Rule::IncludeStat => {
                    // already processed in preprocess_include
                }
                Rule::ModuleDecl => {
                    match self.parse_module_decl(pair, None, &mut use_set, true) {
                        Ok(_) => {}
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                    use_set.clear();
                }
                Rule::ConstantDecl => {
                    let constant = self.build_const_decl(pair, None);
                    self.ast.constants.push(Rc::new(constant));
                }
                Rule::WorldDecl => {
                    if world_count == 0 {
                        match self.parse_world_decl(pair, false) {
                            Ok(_) => {}
                            Err(e) => {
                                if first_error.is_none() {
                                    first_error = Some(e);
                                }
                            }
                        }
                        world_count += 1;
                    } else {
                        // second (or later) occurrence: record a diagnostic (single-span)
                        let s = pair.as_span();
                        let span = SourceSpan::Direct {
                            start: s.start(),
                            end: s.end(),
                        };
                        let err =
                            oorv_error_with_span("Multiple World declarations found", Some(span));
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                        world_count += 1;
                    }
                }
                Rule::EOI => {}
                _ => unreachable!(),
            }
        }
        // Generate a uid signal for each class referenced in a Member type.
        let default_span: SourceSpan = Default::default();
        use std::collections::HashSet;
        let mut member_types: HashSet<String> = HashSet::new();
        for m in self.ast.members.iter() {
            // Member stores its type name in `ty_name`
            let name = m.ty_name.name.clone();
            let short = name.split("::").last().unwrap_or(&name).to_string();
            member_types.insert(short);
        }

        for c in self.ast.classes.iter() {
            // Only generate uid for classes that appear in member types.
            if !member_types.contains(&c.name.name) {
                continue;
            }
            let full_name = if let Some(m) = &c.module_name {
                format!("{}::{}::uid", m.name, c.name.name)
            } else {
                format!("{}::uid", c.name.name)
            };
            let uid_ident = crate::ast::Identifier {
                name: full_name,
                span: default_span,
            };
            let uid_ty = crate::ast::ValueType {
                kind: crate::ast::ValueTypeKind::Named("Int64".to_string()),
                node_id: self.ast.alloc_node_id(),
                span: default_span,
            };
            let uid_signal = crate::ast::Signal {
                node_id: self.ast.alloc_node_id(),
                name: uid_ident,
                module_name: c.module_name.clone(),
                class_name: Some(c.name.clone()),
                params: Vec::new(),
                annotation: uid_ty,
                span: default_span,
            };
            self.ast.signals.push(Rc::new(uid_signal));
        }

        if let Some(e) = first_error {
            return Err(e);
        }

        Ok(self.ast)
    }

    /// Merge base class signals into subclasses (walks inheritance chain).
    fn merge_base_class_signals(ast: &mut OORVAst) -> Result<(), OORVError> {
        // Helper: return class qualified name (module::name or bare name).
        let full_name_of = |c: &crate::ast::ClassDecl| {
            if let Some(mod_name) = &c.module_name {
                format!("{}::{}", mod_name.name, c.name.name)
            } else {
                c.name.name.clone()
            }
        };

        // Reference to the original class list for base-class lookup.
        let orig_classes = ast.classes.clone();

        // Recursively collect signals up the inheritance chain (parents first).
        fn collect_base_signals(
            start: &str,
            classes: &Vec<std::rc::Rc<crate::ast::ClassDecl>>,
            full_name_of: &dyn Fn(&crate::ast::ClassDecl) -> String,
            visited: &mut Vec<String>,
            // current class context used to resolve short names
            curr_module: &Option<crate::ast::Identifier>,
            curr_uses: &std::collections::BTreeSet<String>,
        ) -> Result<Option<Vec<std::rc::Rc<crate::ast::Signal>>>, OORVError> {
            if visited.contains(&start.to_string()) {
                return Err(oorv_error_with_span(
                    &format!("inheritance cycle detected for `{}`", start),
                    None,
                ));
            }
            visited.push(start.to_string());

            // helper to decide whether a candidate class (c) is visible to the
            // current class via same module or via a `use` from the current class.
            fn module_matches(
                candidate_mod: &Option<crate::ast::Identifier>,
                curr_mod: &Option<crate::ast::Identifier>,
                curr_uses: &std::collections::BTreeSet<String>,
            ) -> bool {
                match (candidate_mod, curr_mod) {
                    (Some(cmod), Some(curr)) => {
                        if cmod.name == curr.name {
                            true
                        } else {
                            curr_uses.contains(&cmod.name)
                        }
                    }
                    (Some(cmod), None) => curr_uses.contains(&cmod.name),
                    (None, Some(_)) => false,
                    (None, None) => true,
                }
            }

            // collect visible candidate classes (either full-name match or
            // short-name visible via module/use)
            let mut candidates: Vec<std::rc::Rc<crate::ast::ClassDecl>> = Vec::new();
            for c in classes.iter() {
                let cname = full_name_of(c.as_ref());
                if cname == start {
                    candidates.push(c.clone());
                    continue;
                }
                if c.name.name == start {
                    if module_matches(&c.module_name, curr_module, curr_uses) {
                        candidates.push(c.clone());
                    }
                }
            }

            if candidates.len() > 1 {
                return Err(oorv_error_with_span(
                    &format!(
                        "ambiguous base class name `{}`: multiple candidates found",
                        start
                    ),
                    None,
                ));
            }

            if let Some(cd) = candidates.into_iter().next() {
                // found single candidate: recurse to collect its base signals then its own
                let mut res: Vec<std::rc::Rc<crate::ast::Signal>> = Vec::new();
                if let Some(base) = &cd.base_class {
                    let base_name = base.name.clone();
                    if let Some(mut more) = collect_base_signals(
                        &base_name,
                        classes,
                        full_name_of,
                        visited,
                        curr_module,
                        curr_uses,
                    )? {
                        res.append(&mut more);
                    }
                }
                res.extend(cd.signals.clone());
                return Ok(Some(res));
            }

            Ok(None)
        }

        let mut new_classes: Vec<std::rc::Rc<crate::ast::ClassDecl>> = Vec::new();
        for rc in orig_classes.into_iter() {
            let class = rc.as_ref();
            if let Some(base_ident) = &class.base_class {
                let base_name = base_ident.name.clone();
                let mut visited: Vec<String> = Vec::new();
                let base_signals_opt = collect_base_signals(
                    &base_name,
                    &ast.classes,
                    &full_name_of,
                    &mut visited,
                    &class.module_name,
                    &class.uses,
                )?;
                if let Some(base_signals) = base_signals_opt {
                    // Prepend inherited signals before this class's own signals.
                    // Clone each inherited signal and update its module/class names.
                    let merged = class.signals.clone();
                    let mut final_signals: Vec<std::rc::Rc<crate::ast::Signal>> = Vec::new();
                    for bs in base_signals.into_iter() {
                        let b = bs.as_ref();
                        let new_sig = crate::ast::Signal {
                            node_id: b.node_id,
                            annotation: b.annotation.clone(),
                            name: b.name.clone(),
                            params: b.params.clone(),
                            module_name: class.module_name.clone(),
                            class_name: Some(class.name.clone()),
                            span: b.span,
                        };
                        final_signals.push(std::rc::Rc::new(new_sig));
                    }
                    final_signals.extend(merged.into_iter());
                    let new_class = crate::ast::ClassDecl {
                        name: class.name.clone(),
                        module_name: class.module_name.clone(),
                        base_class: class.base_class.clone(),
                        signals: final_signals,
                        constrains: class.constrains.clone(),
                        uses: class.uses.clone(),
                        node_id: class.node_id,
                        span: class.span,
                    };
                    new_classes.push(std::rc::Rc::new(new_class));
                    continue;
                } else {
                    let full = if let Some(m) = &class.module_name {
                        format!("{}::{}", m.name, class.name.name)
                    } else {
                        class.name.name.clone()
                    };
                    return Err(oorv_error_with_span(
                        &format!("base class `{}` not found for `{}`", base_name, full),
                        Some(class.span),
                    ));
                }
            }
            new_classes.push(rc.clone());
        }
        ast.classes = new_classes;
        Ok(())
    }

    fn convert_class_types_to_basic(ast: &mut OORVAst) -> Result<(), OORVError> {
        use std::collections::HashMap;

        // build lookup: full name and short name -> Rc<ClassDecl>
        let mut class_map: HashMap<String, std::rc::Rc<crate::ast::ClassDecl>> = HashMap::new();
        for c in ast.classes.iter() {
            let full = if let Some(m) = &c.module_name {
                format!("{}::{}", m.name, c.name.name)
            } else {
                c.name.name.clone()
            };
            if let Some(prev) = class_map.get(&full) {
                return Err(oorv_error_with_span(
                    &format!("duplicate class definition `{}`", full),
                    Some(prev.span),
                ));
            }
            class_map.insert(full.clone(), c.clone());
            //class_map.insert(c.name.name.clone(), c.clone());
        }

        fn expand_type_into_signals(
            prefix: &str,
            ty: &crate::ast::ValueType,
            class_map: &HashMap<String, std::rc::Rc<crate::ast::ClassDecl>>,
            next_id: &mut u32,
            visited: &mut Vec<String>,
            // root_module/root_class are the ORIGINAL signal's module/class and must be preserved
            root_module: &Option<crate::ast::Identifier>,
            root_class: &Option<crate::ast::Identifier>,
        ) -> Result<Vec<std::rc::Rc<crate::ast::Signal>>, OORVError> {
            use crate::ast::ValueTypeKind;
            match &ty.kind {
                ValueTypeKind::Named(name) => {
                    // Try to resolve class name from several places in order and
                    // collect candidates. If more than one candidate is found, emit
                    // an ambiguous-class diagnostic error.
                    let mut candidates: Vec<std::rc::Rc<crate::ast::ClassDecl>> = Vec::new();

                    // 1) try current module::name
                    if let Some(m) = root_module {
                        let full = format!("{}::{}", m.name, name);
                        if let Some(cd) = class_map.get(&full) {
                            candidates.push(cd.clone());
                        }
                    }

                    // 2) try each module in current class's `uses` (if we can find the current class decl)
                    if let Some(rc_class) = root_class {
                        let curr_full = if let Some(m) = root_module {
                            format!("{}::{}", m.name, rc_class.name)
                        } else {
                            rc_class.name.clone()
                        };
                        if let Some(curr_cd) = class_map.get(&curr_full) {
                            for used_mod in curr_cd.uses.iter() {
                                let full_used = format!("{}::{}", used_mod, name);
                                if let Some(cd) = class_map.get(&full_used) {
                                    // avoid duplicate insertion of same Rc
                                    if !candidates
                                        .iter()
                                        .any(|e| std::ptr::eq(Rc::as_ptr(e), Rc::as_ptr(cd)))
                                    {
                                        candidates.push(cd.clone());
                                    }
                                }
                            }
                        }
                    }

                    // if multiple candidates found -> diagnostic error
                    if candidates.len() > 1 {
                        return Err(oorv_error_with_span(
                            &format!("ambiguous class name `{}`: multiple candidates found", name),
                            None,
                        ));
                    }

                    if let Some(cd) = candidates.into_iter().next() {
                        // avoid cycles (use the short name as cycle key)
                        if visited.contains(name) {
                            return Ok(vec![]);
                        }
                        visited.push(name.clone());
                        let mut res = Vec::new();
                        for inner in cd.signals.iter() {
                            let field_name = format!("{}::{}", prefix, inner.name.name);
                            // recurse but preserve the original root module/class for all expanded leaves
                            let mut nested = expand_type_into_signals(
                                &field_name,
                                &inner.annotation,
                                class_map,
                                next_id,
                                visited,
                                root_module,
                                root_class,
                            )?;
                            if nested.is_empty() {
                                // create concrete signal from inner, but keep root module/class
                                let new_sig = crate::ast::Signal {
                                    node_id: crate::ast::AstNodeId(*next_id),
                                    annotation: inner.annotation.clone(),
                                    name: crate::ast::Identifier {
                                        name: field_name,
                                        span: inner.name.span,
                                    },
                                    params: inner.params.clone(),
                                    module_name: root_module.clone(),
                                    class_name: root_class.clone(),
                                    span: inner.span,
                                };
                                *next_id += 1;
                                res.push(std::rc::Rc::new(new_sig));
                            } else {
                                res.append(&mut nested);
                            }
                        }
                        visited.pop();
                        return Ok(res);
                    } else {
                        // not a class type: keep as-is
                        let sig = crate::ast::Signal {
                            node_id: crate::ast::AstNodeId(*next_id),
                            annotation: ty.clone(),
                            name: crate::ast::Identifier {
                                name: prefix.to_string(),
                                span: ty.span,
                            },
                            params: Vec::new(),
                            module_name: root_module.clone(),
                            class_name: root_class.clone(),
                            span: ty.span,
                        };
                        *next_id += 1;
                        return Ok(vec![std::rc::Rc::new(sig)]);
                    }
                }
                ValueTypeKind::Tuple(_) | ValueTypeKind::Optional(_) => {
                    // For tuple/optional, treat as a leaf type and create single signal
                    let sig = crate::ast::Signal {
                        node_id: crate::ast::AstNodeId(*next_id),
                        annotation: ty.clone(),
                        name: crate::ast::Identifier {
                            name: prefix.to_string(),
                            span: ty.span,
                        },
                        params: Vec::new(),
                        module_name: root_module.clone(),
                        class_name: root_class.clone(),
                        span: ty.span,
                    };
                    *next_id += 1;
                    Ok(vec![std::rc::Rc::new(sig)])
                }
            }
        }

        // produce new classes vector with expanded signals
        let mut new_classes: Vec<std::rc::Rc<crate::ast::ClassDecl>> = Vec::new();
        // take a snapshot of next id and write back at end to avoid mutable borrow conflicts
        let mut next_id_val: u32 = ast.nodecnts.borrow().0;
        for c in ast.classes.iter() {
            let mut new_signals: Vec<std::rc::Rc<crate::ast::Signal>> = Vec::new();
            for s in c.signals.iter() {
                // if signal type is a class, expand; otherwise keep original
                let mut visited: Vec<String> = Vec::new();
                let expanded = expand_type_into_signals(
                    &s.name.name,
                    &s.annotation,
                    &class_map,
                    &mut next_id_val,
                    &mut visited,
                    &s.module_name,
                    &Some(c.name.clone()),
                )?;
                new_signals.extend(expanded.into_iter());
            }
            let new_class = crate::ast::ClassDecl {
                name: c.name.clone(),
                module_name: c.module_name.clone(),
                base_class: c.base_class.clone(),
                signals: new_signals,
                constrains: c.constrains.clone(),
                uses: c.uses.clone(),
                node_id: c.node_id,
                span: c.span,
            };
            new_classes.push(std::rc::Rc::new(new_class));
        }
        ast.classes = new_classes;
        // write back next id counter
        ast.nodecnts.borrow_mut().0 = next_id_val;
        Ok(())
    }

    /// Add module/class prefix to each class signal's name. Example:
    /// module `Car`, class `Wheel`, field `speed` -> `Car::Wheel_speed`.
    fn qualify_signal_names(ast: &mut OORVAst) {
        use std::rc::Rc;

        let mut new_classes: Vec<Rc<crate::ast::ClassDecl>> = Vec::new();
        for c in ast.classes.iter() {
            let prefix = if let Some(m) = &c.module_name {
                format!("{}::{}", m.name, c.name.name)
            } else {
                c.name.name.clone()
            };

            let mut new_signals: Vec<Rc<crate::ast::Signal>> = Vec::new();
            for s in c.signals.iter() {
                let new_name = format!("{}::{}", prefix, s.name.name);
                let new_ident = crate::ast::Identifier {
                    name: new_name,
                    span: s.name.span,
                };
                let new_sig = crate::ast::Signal {
                    node_id: s.node_id,
                    annotation: s.annotation.clone(),
                    name: new_ident,
                    module_name: s.module_name.clone(),
                    class_name: s.class_name.clone(),
                    params: s.params.clone(),
                    span: s.span,
                };
                new_signals.push(Rc::new(new_sig));
            }

            let new_class = crate::ast::ClassDecl {
                name: c.name.clone(),
                module_name: c.module_name.clone(),
                base_class: c.base_class.clone(),
                signals: new_signals,
                constrains: c.constrains.clone(),
                uses: c.uses.clone(),
                node_id: c.node_id,
                span: c.span,
            };
            new_classes.push(Rc::new(new_class));
        }
        ast.classes = new_classes;
    }

    fn add_signal_constrains(ast: &mut OORVAst) {
        // Iterate signals, skipping any whose name contains "uid".
        for s_rc in ast.signals.iter() {
            let s = s_rc.as_ref();
            let sig_name = &s.name.name;
            if sig_name.contains("uid") {
                continue;
            }

            // output name: append _params
            let out_name = format!("{}_params", sig_name);
            let out_ident = crate::ast::Identifier {
                name: out_name,
                span: Default::default(),
            };

            // single parameter `id`
            let id_param = crate::ast::ParamDecl {
                name: crate::ast::Identifier {
                    name: "id".to_string(),
                    span: Default::default(),
                },
                annotation: None,
                position: 0,
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            // start expression -> module::class::uid (qualify when possible)
            let uid_str = match (&s.module_name, &s.class_name) {
                (Some(m), Some(c)) => format!("{}::{}::uid", m.name, c.name),
                (None, Some(c)) => format!("{}::uid", c.name),
                (Some(m), None) => format!("{}::uid", m.name),
                (None, None) => "uid".to_string(),
            };
            let uid_ident = crate::ast::Identifier {
                name: uid_str.clone(),
                span: Default::default(),
            };
            let uid_expr = crate::ast::ExprNode {
                kind: crate::ast::ExprVariant::Identifier(uid_ident.clone()),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            let start_spec = crate::ast::StartDecl {
                pacing: crate::ast::PacingNode::NotAnnotated(Default::default()),
                condition: None,
                expression: Some(uid_expr.clone()),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            // eval condition: id == uid
            let left_ident = crate::ast::Identifier {
                name: "id".to_string(),
                span: Default::default(),
            };
            let left_expr = crate::ast::ExprNode {
                kind: crate::ast::ExprVariant::Identifier(left_ident),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };
            let right_ident = crate::ast::Identifier {
                name: uid_str.clone(),
                span: Default::default(),
            };
            let right_expr = crate::ast::ExprNode {
                kind: crate::ast::ExprVariant::Identifier(right_ident),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };
            let cond_expr = crate::ast::ExprNode {
                kind: crate::ast::ExprVariant::Binary(
                    crate::ast::BinaryOp::Eq,
                    Box::new(left_expr),
                    Box::new(right_expr),
                ),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            let eval_spec = crate::ast::EvalDecl {
                pacing: crate::ast::PacingNode::NotAnnotated(Default::default()),
                condition: Some(cond_expr),
                expression: Some(crate::ast::ExprNode {
                    kind: crate::ast::ExprVariant::Identifier(s.name.clone()),
                    node_id: ast.alloc_node_id(),
                    span: s.span,
                }),
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            let constrain = crate::ast::Constrain {
                kind: crate::ast::ConstrainKind::Output(out_ident),
                annotation: None,
                params: vec![std::rc::Rc::new(id_param)],
                override_flag: false,
                module_name: s.module_name.clone(),
                class_name: s.class_name.clone(),
                start: Some(start_spec),
                eval: vec![eval_spec],
                end: None,
                level: None,
                node_id: ast.alloc_node_id(),
                span: Default::default(),
            };

            ast.constrains.push(std::rc::Rc::new(constrain));
        }
    }

    fn preprocess_let_idents(spec: &str) -> Result<String, OORVError> {
        let mut let_defs: Vec<(usize, usize, String, Option<String>, Option<String>)> = Vec::new();
        let pairs_for_lets = OORVRule::parse(Rule::Spec, spec).map_err(|e| {
            let d = OORVSpecParser::format_pest_error(e, "<input>", spec);
            OORVError::from(d)
        })?;

        fn collect_let_defs(
            pairs: pest::iterators::Pairs<Rule>,
            defs: &mut Vec<(usize, usize, String, Option<String>, Option<String>)>,
            in_fun: bool,
            curr_module: Option<String>,
            curr_class: Option<String>,
            in_world: bool,
        ) {
            for pair in pairs {
                // If we encounter a FunDecl, descend with in_fun = true so we
                // don't collect let defs inside function bodies.
                if pair.as_rule() == Rule::LetName && !in_fun {
                    for child in pair.into_inner() {
                        if child.as_rule() == Rule::Ident {
                            let span = child.as_span();
                            defs.push((
                                span.start(),
                                span.end(),
                                child.as_str().to_string(),
                                curr_module.clone(),
                                curr_class.clone(),
                            ));
                        }
                    }
                } else {
                    let next_in_fun = in_fun || pair.as_rule() == Rule::FuncDef;
                    let next_in_world = in_world || pair.as_rule() == Rule::WorldDecl;

                    match pair.as_rule() {
                        Rule::ModuleDecl => {
                            // extract module name if present
                            let mut inner = pair.clone().into_inner();
                            if let Some(name_pair) = inner.next() {
                                if name_pair.as_rule() == Rule::Ident {
                                    let mod_name = name_pair.as_str().to_string();
                                    collect_let_defs(
                                        inner,
                                        defs,
                                        next_in_fun,
                                        Some(mod_name),
                                        None,
                                        next_in_world,
                                    );
                                    continue;
                                }
                            }
                            // fallback: descend without module
                            collect_let_defs(
                                pair.into_inner(),
                                defs,
                                next_in_fun,
                                curr_module.clone(),
                                None,
                                next_in_world,
                            );
                        }
                        Rule::TypeDef => {
                            let mut inner = pair.clone().into_inner();
                            if let Some(name_pair) = inner.next() {
                                if name_pair.as_rule() == Rule::Ident {
                                    let class_name = name_pair.as_str().to_string();
                                    collect_let_defs(
                                        inner,
                                        defs,
                                        next_in_fun,
                                        curr_module.clone(),
                                        Some(class_name),
                                        next_in_world,
                                    );
                                    continue;
                                }
                            }
                            collect_let_defs(
                                pair.into_inner(),
                                defs,
                                next_in_fun,
                                curr_module.clone(),
                                curr_class.clone(),
                                next_in_world,
                            );
                        }
                        Rule::WorldDecl => {
                            // WorldDecl: first inner is the world/module name
                            let mut inner = pair.clone().into_inner();
                            if let Some(name_pair) = inner.next() {
                                if name_pair.as_rule() == Rule::Ident {
                                    let mod_name = name_pair.as_str().to_string();
                                    collect_let_defs(
                                        inner,
                                        defs,
                                        next_in_fun,
                                        Some(mod_name),
                                        None,
                                        true,
                                    );
                                    continue;
                                }
                            }
                            collect_let_defs(
                                pair.into_inner(),
                                defs,
                                next_in_fun,
                                curr_module.clone(),
                                curr_class.clone(),
                                true,
                            );
                        }
                        _ => collect_let_defs(
                            pair.into_inner(),
                            defs,
                            next_in_fun,
                            curr_module.clone(),
                            curr_class.clone(),
                            next_in_world,
                        ),
                    }
                }
            }
        }

        collect_let_defs(pairs_for_lets, &mut let_defs, false, None, None, false);

        if let_defs.is_empty() {
            return Ok(spec.to_string());
        }

        // Re-parse to collect all Identifier spans (skip definition positions)
        let pairs_for_idents = OORVRule::parse(Rule::Spec, spec).map_err(|e| {
            let d = OORVSpecParser::format_pest_error(e, "<input>", spec);
            OORVError::from(d)
        })?;
        let mut replacements: Vec<(usize, usize, String)> = Vec::new();

        fn collect_idents(
            pairs: pest::iterators::Pairs<Rule>,
            let_defs: &Vec<(usize, usize, String, Option<String>, Option<String>)>,
            repls: &mut Vec<(usize, usize, String)>,
            in_fun: bool,
            in_world: bool,
            curr_module: Option<String>,
            curr_class: Option<String>,
        ) {
            for pair in pairs {
                // Skip replacements inside function bodies. Allow replacements inside WorldDecl
                // so that we can produce qualified names without '(id)'.
                if pair.as_rule() == Rule::Ident && !in_fun {
                    let s = pair.as_span().start();
                    let e = pair.as_span().end();
                    let name = pair.as_str().to_string();
                    // Only consider let defs that belong to the same module/class context
                    let has_def_in_context = let_defs.iter().any(|(_, _, dn, dm, dc)| {
                        dn == &name
                            && dm.as_ref().map(|s| s.as_str())
                                == curr_module.as_ref().map(|s| s.as_str())
                            && dc.as_ref().map(|s| s.as_str())
                                == curr_class.as_ref().map(|s| s.as_str())
                    });
                    if has_def_in_context {
                        // skip if this ident is the LetStart definition
                        let is_def = let_defs
                            .iter()
                            .any(|(ds, de, dn, _, _)| *ds == s && *de == e && dn == &name);
                        if !is_def {
                            // build qualified name prefix if available
                            let rep = if in_world {
                                if let (Some(m), Some(c)) = (&curr_module, &curr_class) {
                                    format!("{}::{}::{}", m, c, name)
                                } else if let Some(m) = &curr_module {
                                    format!("{}::{}", m, name)
                                } else if let Some(c) = &curr_class {
                                    format!("{}::{}", c, name)
                                } else {
                                    name.clone()
                                }
                            } else {
                                if let (Some(m), Some(c)) = (&curr_module, &curr_class) {
                                    format!("{}::{}::{}(id)", m, c, name)
                                } else if let Some(m) = &curr_module {
                                    format!("{}::{}(id)", m, name)
                                } else if let Some(c) = &curr_class {
                                    format!("{}::{}(id)", c, name)
                                } else {
                                    format!("{}(id)", name)
                                }
                            };
                            repls.push((s, e, rep));
                        }
                    }
                }
                let next_in_fun = in_fun || pair.as_rule() == Rule::FuncDef;
                let next_in_world = in_world || pair.as_rule() == Rule::WorldDecl;

                // update module/class context when descending into ModuleDecl/ClassDecl
                match pair.as_rule() {
                    Rule::ModuleDecl => {
                        // extract module name if present
                        let mut inner = pair.clone().into_inner();
                        if let Some(name_pair) = inner.next() {
                            if name_pair.as_rule() == Rule::Ident {
                                let mod_name = name_pair.as_str().to_string();
                                collect_idents(
                                    inner,
                                    let_defs,
                                    repls,
                                    next_in_fun,
                                    next_in_world,
                                    Some(mod_name),
                                    None,
                                );
                                continue;
                            }
                        }
                        // fallback: descend without module
                        collect_idents(
                            pair.into_inner(),
                            let_defs,
                            repls,
                            next_in_fun,
                            next_in_world,
                            curr_module.clone(),
                            None,
                        );
                    }
                    Rule::TypeDef => {
                        let mut inner = pair.clone().into_inner();
                        if let Some(name_pair) = inner.next() {
                            if name_pair.as_rule() == Rule::Ident {
                                let class_name = name_pair.as_str().to_string();
                                collect_idents(
                                    inner,
                                    let_defs,
                                    repls,
                                    next_in_fun,
                                    next_in_world,
                                    curr_module.clone(),
                                    Some(class_name),
                                );
                                continue;
                            }
                        }
                        collect_idents(
                            pair.into_inner(),
                            let_defs,
                            repls,
                            next_in_fun,
                            next_in_world,
                            curr_module.clone(),
                            curr_class.clone(),
                        );
                    }
                    Rule::WorldDecl => {
                        // WorldDecl: first inner is the world/module name
                        let mut inner = pair.clone().into_inner();
                        if let Some(name_pair) = inner.next() {
                            if name_pair.as_rule() == Rule::Ident {
                                let mod_name = name_pair.as_str().to_string();
                                collect_idents(
                                    inner,
                                    let_defs,
                                    repls,
                                    next_in_fun,
                                    next_in_world,
                                    Some(mod_name),
                                    None,
                                );
                                continue;
                            }
                        }
                        collect_idents(
                            pair.into_inner(),
                            let_defs,
                            repls,
                            next_in_fun,
                            next_in_world,
                            curr_module.clone(),
                            curr_class.clone(),
                        );
                    }
                    _ => collect_idents(
                        pair.into_inner(),
                        let_defs,
                        repls,
                        next_in_fun,
                        next_in_world,
                        curr_module.clone(),
                        curr_class.clone(),
                    ),
                }
            }
        }

        collect_idents(
            pairs_for_idents,
            &let_defs,
            &mut replacements,
            false,
            false,
            None,
            None,
        );

        if replacements.is_empty() {
            return Ok(spec.to_string());
        }

        // Apply replacements in reverse order to avoid offset shifts
        replacements.sort_by(|a, b| b.0.cmp(&a.0));
        let mut spec_mut = spec.to_string();
        for (s, e, rep) in replacements {
            spec_mut.replace_range(s..e, &rep);
        }

        Ok(spec_mut)
    }

    fn check_member_types_and_uses(ast: &mut OORVAst) -> Result<(), OORVError> {
        use crate::ast::ValueTypeKind;
        // helper to get full name
        let full_name_of = |c: &crate::ast::ClassDecl| {
            if let Some(m) = &c.module_name {
                format!("{}::{}", m.name, c.name.name)
            } else {
                c.name.name.clone()
            }
        };

        // helper similar to other places to decide visibility
        fn module_matches(
            candidate_mod: &Option<crate::ast::Identifier>,
            curr_mod: &Option<crate::ast::Identifier>,
            curr_uses: &std::collections::BTreeSet<String>,
        ) -> bool {
            match (candidate_mod, curr_mod) {
                (Some(cmod), Some(curr)) => {
                    if cmod.name == curr.name {
                        true
                    } else {
                        curr_uses.contains(&cmod.name)
                    }
                }
                (Some(cmod), None) => curr_uses.contains(&cmod.name),
                (None, Some(_)) => false,
                (None, None) => true,
            }
        }

        let mut diagnostics: Vec<OORVError> = Vec::new();

        // iterate by index so we can replace members with updated `ty_name` when resolved
        for i in 0..ast.members.len() {
            let mrc = ast.members[i].clone();
            let m = mrc.as_ref();
            match &m.annotation {
                Some(typ) => match &typ.kind {
                    ValueTypeKind::Named(name) => {
                        // collect visible candidate classes
                        let mut candidates: Vec<std::rc::Rc<crate::ast::ClassDecl>> = Vec::new();
                        for c in ast.classes.iter() {
                            let full = full_name_of(c.as_ref());
                            if full == *name {
                                candidates.push(c.clone());
                                continue;
                            }
                            if c.name.name == *name {
                                if module_matches(&c.module_name, &None, &m.uses) {
                                    candidates.push(c.clone());
                                }
                            }
                        }

                        if candidates.is_empty() {
                            diagnostics.push(oorv_error_with_span(
                                &format!(
                                    "type `{}` for member `{}` not found in uses",
                                    name, m.name.name
                                ),
                                Some(m.span),
                            ));
                            continue;
                        }

                        if candidates.len() > 1 {
                            diagnostics.push(oorv_error_with_span(
                                &format!(
                                "ambiguous type `{}` for member `{}`: multiple candidates found",
                                name, m.name.name
                            ),
                                Some(m.span),
                            ));
                            continue;
                        }

                        // Exactly one candidate: update member.ty_name to include module prefix if present
                        if let Some(cd_rc) = candidates.into_iter().next() {
                            let cd = cd_rc.as_ref();
                            let new_ty_name_str = if let Some(mod_ident) = &cd.module_name {
                                format!("{}::{}", mod_ident.name, cd.name.name)
                            } else {
                                name.clone()
                            };
                            let new_ty_name = crate::ast::Identifier {
                                name: new_ty_name_str,
                                span: m.ty_name.span,
                            };
                            let new_member = crate::ast::Member {
                                name: m.name.clone(),
                                annotation: m.annotation.clone(),
                                uses: m.uses.clone(),
                                ty_name: new_ty_name,
                                params: m.params.clone(),
                                node_id: m.node_id,
                                span: m.span,
                            };
                            ast.members[i] = std::rc::Rc::new(new_member);
                        }
                    }
                    _ => {
                        // non-simple types are not subject to this check
                    }
                },
                None => {
                    // no annotation to check
                }
            }
        }

        if !diagnostics.is_empty() {
            return Err(diagnostics.remove(0));
        }

        Ok(())
    }

    /// Rewrite member references inside Constrain expressions to use the member's
    /// `ty_name` (fully-qualified) as prefix. Example: `car.a` -> `Car::Car::a`.
    fn rewrite_member_refs(ast: &mut OORVAst) {
        use std::collections::{HashMap, HashSet};

        let mut member_map: HashMap<String, String> = HashMap::new();
        for m in ast.members.iter() {
            member_map.insert(m.name.name.clone(), m.ty_name.name.clone());
        }

        // helper: flatten a Field chain into a combined name like "a::b::c"
        fn flatten_ident_chain(expr: &crate::ast::ExprNode) -> Option<String> {
            match &expr.kind {
                crate::ast::ExprVariant::Identifier(id) => Some(id.name.clone()),
                crate::ast::ExprVariant::Field(inner, id) => {
                    if let Some(base) = flatten_ident_chain(inner) {
                        Some(format!("{}::{}", base, id.name))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }

        // replace member prefix if present
        fn replace_prefix(name: &str, map: &HashMap<String, String>) -> String {
            for (k, v) in map.iter() {
                if name == k {
                    return v.clone();
                } else if name.starts_with(&format!("{}::", k)) && name.contains("uid") {
                    let rest = &name[k.len()..];
                    return format!("{}{}", v, rest);
                } else if name.starts_with(&format!("{}::", k)) && !name.contains("uid") {
                    let rest = &name[k.len()..];
                    return format!("{}{}_params", v, rest);
                }
            }
            name.to_string()
        }

        fn quantifier_arg_name(
            name: &str,
            quantified_bindings: &HashSet<String>,
        ) -> Option<String> {
            let head = name.split("::").next().unwrap_or(name);
            quantified_bindings.contains(head).then(|| head.to_string())
        }

        fn param_call_expr(
            fn_name: String,
            original_name: &str,
            span: crate::ast::SourceSpan,
            node_id: crate::ast::AstNodeId,
            quantified_bindings: &HashSet<String>,
            ast: &crate::ast::OORVAst,
        ) -> crate::ast::ExprNode {
            use crate::ast::ExprVariant;

            let fn_ident = crate::ast::Identifier {
                name: fn_name,
                span,
            };
            let fname = crate::ast::FuncLabel {
                name: fn_ident,
                arg_names: vec![None],
            };
            let arg_expr =
                if let Some(arg_name) = quantifier_arg_name(original_name, quantified_bindings) {
                    crate::ast::ExprNode {
                        kind: ExprVariant::Identifier(crate::ast::Identifier {
                            name: arg_name,
                            span,
                        }),
                        node_id: ast.alloc_node_id(),
                        span,
                    }
                } else {
                    let lit = crate::ast::TokenLiteral {
                        kind: crate::ast::LiteralKind::Number("0".to_string(), None),
                        node_id: ast.alloc_node_id(),
                        span,
                    };
                    crate::ast::ExprNode {
                        kind: ExprVariant::Literal(lit),
                        node_id: ast.alloc_node_id(),
                        span,
                    }
                };

            crate::ast::ExprNode {
                kind: ExprVariant::Function(fname, Vec::new(), vec![arg_expr]),
                node_id,
                span,
            }
        }

        // Recursively transform ExprNode
        fn transform_expr(
            expr: &crate::ast::ExprNode,
            map: &HashMap<String, String>,
            quantified_bindings: &HashSet<String>,
            ast: &crate::ast::OORVAst,
        ) -> crate::ast::ExprNode {
            use crate::ast::ExprVariant;

            let id = expr.node_id;
            let span = expr.span.clone();
            match &expr.kind {
                ExprVariant::Identifier(i) => {
                    if quantified_bindings.contains(&i.name) {
                        return crate::ast::ExprNode {
                            kind: ExprVariant::Identifier(i.clone()),
                            node_id: id,
                            span,
                        };
                    }
                    let new_name = replace_prefix(&i.name, map);
                    // If this looks like a parameter-accessor (suffix "_params"),
                    // turn it into a Function call.  Quantified object access such
                    // as `car.speed` must retain `car` as the object parameter;
                    // outside quantified scope this remains the legacy fallback.
                    if new_name.ends_with("_params") {
                        param_call_expr(new_name, &i.name, span, id, quantified_bindings, ast)
                    } else {
                        crate::ast::ExprNode {
                            kind: ExprVariant::Identifier(crate::ast::Identifier {
                                name: new_name,
                                span: i.span.clone(),
                            }),
                            node_id: id,
                            span,
                        }
                    }
                }
                ExprVariant::Field(_, _) => {
                    if let Some(flat) = flatten_ident_chain(expr) {
                        let replaced = replace_prefix(&flat, map);
                        // If the flattened name denotes params accessor, convert to Function
                        if replaced.ends_with("_params") {
                            param_call_expr(replaced, &flat, span, id, quantified_bindings, ast)
                        } else {
                            crate::ast::ExprNode {
                                kind: ExprVariant::Identifier(crate::ast::Identifier {
                                    name: replaced,
                                    span: span.clone(),
                                }),
                                node_id: id,
                                span,
                            }
                        }
                    } else {
                        // fallback: recursively transform inner/field parts
                        match &expr.kind {
                            ExprVariant::Field(inner, f) => {
                                let new_inner =
                                    transform_expr(inner, map, quantified_bindings, ast);
                                crate::ast::ExprNode {
                                    kind: ExprVariant::Field(Box::new(new_inner), f.clone()),
                                    node_id: id,
                                    span,
                                }
                            }
                            _ => expr.clone(),
                        }
                    }
                }
                ExprVariant::Binary(op, l, r) => {
                    let nl = transform_expr(l, map, quantified_bindings, ast);
                    let nr = transform_expr(r, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Binary(*op, Box::new(nl), Box::new(nr)),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Unary(u, inner) => {
                    let ni = transform_expr(inner, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Unary(*u, Box::new(ni)),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Ite(a, b, c) => {
                    let na = transform_expr(a, map, quantified_bindings, ast);
                    let nb = transform_expr(b, map, quantified_bindings, ast);
                    let nc = transform_expr(c, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Ite(Box::new(na), Box::new(nb), Box::new(nc)),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Bracket(inner) => {
                    let ni = transform_expr(inner, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Bracket(Box::new(ni)),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Tuple(el) => {
                    let new_el: Vec<crate::ast::ExprNode> = el
                        .iter()
                        .map(|e| transform_expr(e, map, quantified_bindings, ast))
                        .collect();
                    crate::ast::ExprNode {
                        kind: ExprVariant::Tuple(new_el),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Function(fnname, tys, args) => {
                    let new_args: Vec<crate::ast::ExprNode> = args
                        .iter()
                        .map(|a| transform_expr(a, map, quantified_bindings, ast))
                        .collect();
                    crate::ast::ExprNode {
                        kind: ExprVariant::Function(fnname.clone(), tys.clone(), new_args),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Method(inner, name, tys, args) => {
                    let ni = transform_expr(inner, map, quantified_bindings, ast);
                    let new_args: Vec<crate::ast::ExprNode> = args
                        .iter()
                        .map(|a| transform_expr(a, map, quantified_bindings, ast))
                        .collect();
                    crate::ast::ExprNode {
                        kind: ExprVariant::Method(
                            Box::new(ni),
                            name.clone(),
                            tys.clone(),
                            new_args,
                        ),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Default(a, b) => {
                    let na = transform_expr(a, map, quantified_bindings, ast);
                    let nb = transform_expr(b, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Default(Box::new(na), Box::new(nb)),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Shift(a, off) => {
                    let na = transform_expr(a, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Shift(Box::new(na), *off),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::SignalAccess(a, k) => {
                    let na = transform_expr(a, map, quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::SignalAccess(Box::new(na), *k),
                        node_id: id,
                        span,
                    }
                }
                ExprVariant::Quantified(q, idents1, idents2, body) => {
                    let mut scoped_map = map.clone();
                    let mut scoped_quantified_bindings = quantified_bindings.clone();
                    let mut seen = std::collections::HashSet::new();
                    for (binding, domain) in idents1.iter().zip(idents2.iter()) {
                        if seen.insert(binding.name.clone()) {
                            let mapped_domain = map
                                .get(&domain.name)
                                .cloned()
                                .unwrap_or_else(|| domain.name.clone());
                            scoped_map.insert(binding.name.clone(), mapped_domain);
                            scoped_quantified_bindings.insert(binding.name.clone());
                        }
                    }
                    let new_body =
                        transform_expr(body, &scoped_map, &scoped_quantified_bindings, ast);
                    crate::ast::ExprNode {
                        kind: ExprVariant::Quantified(
                            q.clone(),
                            idents1.clone(),
                            idents2.clone(),
                            Box::new(new_body),
                        ),
                        node_id: id,
                        span,
                    }
                }
                // literals and missing expressions are unchanged
                ExprVariant::Literal(_) | ExprVariant::MissingExpr => expr.clone(),
            }
        }

        // iterate and replace inside each constrain
        for idx in 0..ast.constrains.len() {
            let c = ast.constrains[idx].as_ref().clone();
            let mut new_c = c.clone();
            let quantified_bindings = HashSet::new();

            if let Some(sp) = &c.start {
                let mut spn = sp.clone();
                if let Some(e) = &sp.expression {
                    spn.expression =
                        Some(transform_expr(e, &member_map, &quantified_bindings, ast));
                }
                if let Some(cond) = &sp.condition {
                    spn.condition =
                        Some(transform_expr(cond, &member_map, &quantified_bindings, ast));
                }
                new_c.start = spn.into();
            }

            // transform eval specs
            let mut new_eval: Vec<crate::ast::EvalDecl> = Vec::new();
            for ev in c.eval.iter() {
                let mut nev = ev.clone();
                if let Some(e) = &ev.expression {
                    nev.expression =
                        Some(transform_expr(e, &member_map, &quantified_bindings, ast));
                }
                if let Some(cond) = &ev.condition {
                    nev.condition =
                        Some(transform_expr(cond, &member_map, &quantified_bindings, ast));
                }
                new_eval.push(nev);
            }
            new_c.eval = new_eval;

            // transform end spec
            if let Some(cl) = &c.end {
                let mut ncl = cl.clone();
                ncl.condition =
                    transform_expr(&cl.condition, &member_map, &quantified_bindings, ast);
                new_c.end = Some(ncl);
            }

            // replace in ast
            ast.constrains[idx] = std::rc::Rc::new(new_c);
        }
    }

    fn preprocess_format_macros(spec: &str) -> Result<String, OORVError> {
        let macros = ["info!", "alert!", "violation!"];
        let mut out = String::with_capacity(spec.len());
        let s = spec;
        let bytes = s.as_bytes();
        let mut idx = 0usize;
        let n = s.len();

        while idx < n {
            // find next macro occurrence among the three
            let mut next_pos: Option<usize> = None;
            let mut next_name: Option<&str> = None;
            for &m in macros.iter() {
                if let Some(p) = s[idx..].find(m) {
                    let abs = idx + p;
                    if next_pos.map_or(true, |cur| abs < cur) {
                        next_pos = Some(abs);
                        next_name = Some(m);
                    }
                }
            }

            if let Some(start) = next_pos {
                // copy prefix
                out.push_str(&s[idx..start]);
                let mac = next_name.unwrap();
                let after_macro = start + mac.len();
                // scan to first '"' after possibly some whitespace
                let mut j = after_macro;
                while j < n && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j >= n || bytes[j] as char != '"' {
                    // no string literal follows: copy macro name and advance
                    out.push_str(&s[start..after_macro]);
                    idx = after_macro;
                    continue;
                }
                let open_q = j; // index of '"'
                                // find closing quote, respecting escapes
                let mut k = open_q + 1;
                let mut end = false;
                while k < n {
                    if bytes[k] == b'\\' {
                        k += 2;
                    } else if bytes[k] == b'"' {
                        end = true;
                        break;
                    } else {
                        k += 1;
                    }
                }
                if !end {
                    return Err(oorv_error_with_span(
                        "unterminated string literal in macro format preprocessing",
                        None,
                    ));
                }
                // literal content between open_q+1 .. k
                let lit = &s[(open_q + 1)..k];

                // scan lit for { ... } occurrences and collect args
                let mut new_lit = String::with_capacity(lit.len());
                let mut args: Vec<String> = Vec::new();
                let mut p = 0usize;
                let l = lit.len();
                let lit_bytes = lit.as_bytes();
                while p < l {
                    if lit_bytes[p] == b'\\' {
                        // escaped char: copy as-is (keep backslash and next char)
                        if p + 1 < l {
                            new_lit.push_str(&lit[p..p + 2]);
                            p += 2;
                        } else {
                            p += 1;
                        }
                    } else if lit_bytes[p] == b'{' {
                        // find closing '}'
                        let mut q = p + 1;
                        let mut found = false;
                        while q < l {
                            if lit_bytes[q] == b'}' {
                                found = true;
                                break;
                            }
                            q += 1;
                        }
                        if found {
                            let inner = &lit[(p + 1)..q].trim();
                            if !inner.is_empty() {
                                new_lit.push_str("{}");
                                args.push(inner.to_string());
                                p = q + 1;
                                continue;
                            } else {
                                new_lit.push_str("{}");
                                p = q + 1;
                                continue;
                            }
                        } else {
                            // no matching brace: treat as literal
                            new_lit.push('{');
                            p += 1;
                        }
                    } else {
                        // normal char: append next char
                        let ch = lit[p..].chars().next().unwrap();
                        new_lit.push(ch);
                        p += ch.len_utf8();
                    }
                }

                // write macro prefix and the new literal
                out.push_str(&s[start..open_q]); // includes macro name and intervening whitespace
                out.push('"');
                out.push_str(&new_lit);
                out.push('"');

                // if we collected args, append .format(...)
                if !args.is_empty() {
                    out.push_str(".format(");
                    for (ii, a) in args.iter().enumerate() {
                        if ii > 0 {
                            out.push(',');
                        }
                        out.push_str(a);
                    }
                    out.push(')');
                }

                // advance idx to character after closing quote
                idx = k + 1;
            } else {
                // no more macros: copy rest and break
                out.push_str(&s[idx..]);
                break;
            }
        }

        Ok(out)
    }

    fn build_const_decl(
        &self,
        node: Pair<'_, Rule>,
        _parent_module: Option<Identifier>,
    ) -> ConstDecl {
        debug_assert_eq!(node.as_rule(), Rule::ConstantDecl);
        let full_span = node.as_span();
        let span = SourceSpan::Direct {
            start: full_span.start(),
            end: full_span.end(),
        };

        let mut inner = node.into_inner();
        let ty_pair = inner.next().expect("expected type in const decl");
        let ident_pair = inner.next().expect("expected identifier in const decl");
        let val_pair = inner.next().expect("expected value in const decl");

        let ann = self.resolve_type(ty_pair);
        let name = self.extract_ident(&ident_pair);
        let value = self.parse_value_literal(val_pair);

        ConstDecl {
            node_id: self.ast.alloc_node_id(),
            name,
            annotation: Some(ann),
            value,
            span,
        }
    }

    fn collect_signals_block(
        &self,
        pair: Pair<'_, Rule>,
        module: Option<Identifier>,
        class: Option<Identifier>,
    ) -> Result<Vec<Rc<Signal>>, OORVError> {
        if pair.as_rule() != Rule::StreamBlock {
            let s = pair.as_span();
            let span_direct = SourceSpan::Direct {
                start: s.start(),
                end: s.end(),
            };
            return Err(oorv_error_with_span(
                "internal parser error: expected SignalBlock",
                Some(span_direct),
            ));
        }

        let list_pair = match pair.into_inner().next() {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let signals: Vec<Rc<Signal>> = list_pair
            .into_inner()
            .filter(|p| p.as_rule() == Rule::StreamItem)
            .map(|decl_pair| {
                let mut sig = self.build_signal_decl(decl_pair);
                sig.module_name = module.clone();
                sig.class_name = class.clone();
                Rc::new(sig)
            })
            .collect();

        Ok(signals)
    }

    fn build_signal_decl(&self, pair: Pair<'_, Rule>) -> Signal {
        if pair.as_rule() != Rule::StreamItem {
            panic!(
                "internal parser bug: build_signal_decl called with {:?}",
                pair.as_rule()
            );
        }

        let span = {
            let s = pair.as_span();
            SourceSpan::Direct {
                start: s.start(),
                end: s.end(),
            }
        };

        let mut parts = pair.into_inner();
        let tpair = parts.next().expect("signal decl missing type");
        let npair = parts.next().expect("signal decl missing name");

        let ann = self.resolve_type(tpair);
        let name = self.extract_ident(&npair);

        // look ahead for optional parameter list
        let mut params: Vec<ParamDecl> = Vec::new();
        if let Some(peek) = parts.next() {
            if peek.as_rule() == Rule::ParamList {
                params = self.gather_parameters(peek.into_inner());
            }
        }

        Signal {
            node_id: self.ast.alloc_node_id(),
            annotation: ann,
            name,
            params: params.into_iter().map(Rc::new).collect(),
            module_name: None,
            class_name: None,
            span,
        }
    }

    fn gather_parameters(&self, param_list: Pairs<'_, Rule>) -> Vec<ParamDecl> {
        param_list
            .into_iter()
            .enumerate()
            .map(|(idx, p)| {
                if p.as_rule() != Rule::ParamItem {
                    panic!("unexpected rule in parameter list: {:?}", p.as_rule());
                }
                let span = p.as_span();
                let mut inner = p.into_inner();
                let name_pair = inner.next().expect("parameter missing name");
                let name = self.extract_ident(&name_pair);
                let ann = inner.next().map(|tp| {
                    assert_eq!(tp.as_rule(), Rule::TypeRef);
                    self.resolve_type(tp)
                });
                ParamDecl {
                    name,
                    annotation: ann,
                    position: idx,
                    node_id: self.ast.alloc_node_id(),
                    span: SourceSpan::Direct {
                        start: span.start(),
                        end: span.end(),
                    },
                }
            })
            .collect()
    }

    fn parse_module_decl(
        &mut self,
        pair: Pair<'_, Rule>,
        parent_module: Option<Identifier>,
        use_set: &mut BTreeSet<String>,
        parameter_flag: bool,
    ) -> Result<(), OORVError> {
        assert_eq!(pair.as_rule(), Rule::ModuleDecl);
        let mut inner = pair.into_inner();
        // first inner should be Identifier (module name)
        let name_pair = inner.next().expect("Expected Identifier in ModuleDecl");
        let name = self.extract_ident(&name_pair);
        // compose full module name with parent if present
        let full_module: Identifier = if let Some(parent) = parent_module {
            let combined = format!("{}::{}", parent.name, name.name);
            let span = Self::union_span(&parent.span, &name.span);
            Identifier {
                name: combined,
                span,
            }
        } else {
            name.clone()
        };

        let mut first_error: Option<OORVError> = None;

        for child in inner {
            match child.as_rule() {
                Rule::UseStat => {
                    // parse `use IDENT;` and record the ident name into local set
                    let mut inner_use = child.into_inner();
                    if let Some(ident_pair) = inner_use.next() {
                        let imp_ident = self.extract_ident(&ident_pair);
                        use_set.insert(imp_ident.name.clone());
                    }
                }
                Rule::ModuleDecl => {
                    // nested module: recurse with a cloned use_set so child's imports
                    // don't mutate the parent's set (child should inherit but not leak)
                    let mut child_use_set = use_set.clone();
                    match self.parse_module_decl(
                        child,
                        Some(full_module.clone()),
                        &mut child_use_set,
                        parameter_flag,
                    ) {
                        Ok(()) => {}
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                Rule::TypeDef => {
                    match self.parse_class(
                        child,
                        Some(full_module.clone()),
                        &*use_set,
                        parameter_flag,
                    ) {
                        Ok(class) => {
                            self.ast.classes.push(Rc::new(class));
                        }
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                Rule::FuncDef => match self.parse_fun_decl(child, Some(full_module.clone())) {
                    Ok(gm) => self.ast.functions.push(gm),
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                },
                Rule::CheckBlock => {
                    match self.parse_constrain_block(
                        child,
                        Some(full_module.clone()),
                        None,
                        parameter_flag,
                    ) {
                        Ok(constrain) => self.ast.constrains.extend(constrain),
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(())
    }

    fn parse_member_decl(&self, pair: Pair<'_, Rule>, use_set: &BTreeSet<String>) -> Member {
        assert_eq!(pair.as_rule(), Rule::MemberDecl);
        let span = {
            let s = pair.as_span();
            SourceSpan::Direct {
                start: s.start(),
                end: s.end(),
            }
        };
        let mut inner = pair.into_inner();
        let name_pair = inner.next().expect("Expected Identifier in MemberDecl");
        let type_pair = inner.next().expect("Expected Type in MemberDecl");
        // Extract the type name into ty_name from type_pair's raw text and span.
        let ty_name = {
            let s = type_pair.as_span();
            crate::ast::Identifier {
                name: type_pair.as_str().to_string(),
                span: SourceSpan::Direct {
                    start: s.start(),
                    end: s.end(),
                },
            }
        };
        let ty = self.resolve_type(type_pair);
        let name = self.extract_ident(&name_pair);
        let params: Vec<ParamDecl> = Vec::new();
        Member {
            name,
            annotation: Some(ty),
            uses: use_set.clone(),
            ty_name,
            params: params.into_iter().map(Rc::new).collect(),
            node_id: self.ast.alloc_node_id(),
            span,
        }
    }

    fn parse_world_decl(
        &mut self,
        pair: Pair<'_, Rule>,
        parameter_flag: bool,
    ) -> Result<(), OORVError> {
        assert_eq!(pair.as_rule(), Rule::WorldDecl);
        let mut first_error: Option<OORVError> = None;
        let mut inner = pair.into_inner();
        // first inner should be Identifier (module name)
        let name_pair = inner.next().expect("Expected Identifier in ModuleDecl");
        let name: Identifier = self.extract_ident(&name_pair);

        let mut use_set: BTreeSet<String> = BTreeSet::new();
        for child in inner {
            match child.as_rule() {
                Rule::UseStat => {
                    // parse `use IDENT;` and record the ident name into local set
                    let mut inner_use = child.into_inner();
                    if let Some(ident_pair) = inner_use.next() {
                        let imp_ident = self.extract_ident(&ident_pair);
                        use_set.insert(imp_ident.name.clone());
                    }
                }
                Rule::MemberDecl => {
                    let member = self.parse_member_decl(child, &use_set);
                    self.ast.members.push(Rc::new(member));
                }
                Rule::FuncDef => match self.parse_fun_decl(child, None) {
                    Ok(gm) => self.ast.functions.push(gm),
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                },
                Rule::CheckBlock => {
                    match self.parse_constrain_block(
                        child,
                        Some(name.clone()),
                        None,
                        parameter_flag,
                    ) {
                        Ok(constrain) => {
                            // record start index of new constraints
                            let start_len = self.ast.constrains.len();
                            // extend with parsed constraints
                            self.ast.constrains.extend(constrain);

                            // Helper: flatten an Identifier/Field chain into a single string like "A::B::c"
                            fn flatten_ident_chain(expr: &crate::ast::ExprNode) -> Option<String> {
                                use crate::ast::ExprVariant;
                                match &expr.kind {
                                    ExprVariant::Identifier(id) => Some(id.name.clone()),
                                    ExprVariant::Field(inner, id) => {
                                        if let Some(base) = flatten_ident_chain(inner) {
                                            Some(format!("{}::{}", base, id.name))
                                        } else {
                                            None
                                        }
                                    }
                                    _ => None,
                                }
                            }

                            // Recursive search: returns true if any SignalAccess with an Identifier containing "uid" is found
                            fn expr_contains_signal_access_uid(
                                expr: &crate::ast::ExprNode,
                            ) -> bool {
                                use crate::ast::ExprVariant;
                                match &expr.kind {
                                    ExprVariant::SignalAccess(inner, _) => {
                                        if let Some(flat) = flatten_ident_chain(inner) {
                                            return flat.contains("uid");
                                        }
                                        false
                                    }
                                    ExprVariant::Field(inner, _)
                                    | ExprVariant::Bracket(inner)
                                    | ExprVariant::Unary(_, inner)
                                    | ExprVariant::Default(inner, _)
                                    | ExprVariant::Shift(inner, _) => {
                                        expr_contains_signal_access_uid(inner)
                                    }
                                    ExprVariant::Binary(_, l, r) | ExprVariant::Ite(l, r, _) => {
                                        expr_contains_signal_access_uid(l)
                                            || expr_contains_signal_access_uid(r)
                                    }
                                    ExprVariant::Tuple(el) => {
                                        el.iter().any(|e| expr_contains_signal_access_uid(e))
                                    }
                                    ExprVariant::Function(_, _, args)
                                    | ExprVariant::Method(_, _, _, args) => {
                                        args.iter().any(|a| expr_contains_signal_access_uid(a))
                                    }
                                    ExprVariant::Quantified(_, _, _, body) => {
                                        expr_contains_signal_access_uid(body)
                                    }
                                    ExprVariant::Literal(_)
                                    | ExprVariant::MissingExpr
                                    | ExprVariant::Identifier(_) => false,
                                }
                            }

                            // Recursive traversal to find Method calls named "format" that lack any uid SignalAccess in their args
                            fn find_format_without_uid(
                                expr: &crate::ast::ExprNode,
                            ) -> Option<crate::ast::SourceSpan> {
                                use crate::ast::ExprVariant;
                                match &expr.kind {
                                    ExprVariant::Method(_, fname, _, args) => {
                                        if fname.name.name.as_str() == "format" {
                                            // If none of the args contains a SignalAccess with uid -> violation
                                            let has_uid = args
                                                .iter()
                                                .any(|a| expr_contains_signal_access_uid(a));
                                            if !has_uid {
                                                return Some(expr.span.clone());
                                            }
                                        }
                                        // also continue searching inside args
                                        for a in args.iter() {
                                            if let Some(s) = find_format_without_uid(a) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Function(_, _, args) => {
                                        for a in args.iter() {
                                            if let Some(s) = find_format_without_uid(a) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Binary(_, l, r) => find_format_without_uid(l)
                                        .or_else(|| find_format_without_uid(r)),
                                    ExprVariant::Unary(_, inner)
                                    | ExprVariant::Bracket(inner)
                                    | ExprVariant::Default(inner, _)
                                    | ExprVariant::Shift(inner, _) => {
                                        find_format_without_uid(inner)
                                    }
                                    ExprVariant::Ite(a, b, c) => find_format_without_uid(a)
                                        .or_else(|| find_format_without_uid(b))
                                        .or_else(|| find_format_without_uid(c)),
                                    ExprVariant::Tuple(el) => {
                                        for e in el.iter() {
                                            if let Some(s) = find_format_without_uid(e) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Field(inner, _) => find_format_without_uid(inner),
                                    ExprVariant::SignalAccess(inner, _) => {
                                        find_format_without_uid(inner)
                                    }
                                    ExprVariant::Quantified(_, _, _, body) => {
                                        find_format_without_uid(body)
                                    }
                                    ExprVariant::Literal(_)
                                    | ExprVariant::MissingExpr
                                    | ExprVariant::Identifier(_) => None,
                                }
                            }

                            // Recursive traversal to find any Method/Function calls named "format"
                            fn find_any_format(
                                expr: &crate::ast::ExprNode,
                            ) -> Option<crate::ast::SourceSpan> {
                                use crate::ast::ExprVariant;
                                match &expr.kind {
                                    ExprVariant::Method(_, fname, _, _args) => {
                                        if fname.name.name.as_str() == "format" {
                                            return Some(expr.span.clone());
                                        }
                                        // continue searching inside args/receiver
                                        for a in _args.iter() {
                                            if let Some(s) = find_any_format(a) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Function(_, _, args) => {
                                        // function named format
                                        // function name is in the first tuple element; check its string
                                        // here name resolution stores Identifier in the first field
                                        // but to be safe, search args as well
                                        for a in args.iter() {
                                            if let Some(s) = find_any_format(a) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Binary(_, l, r) => {
                                        find_any_format(l).or_else(|| find_any_format(r))
                                    }
                                    ExprVariant::Unary(_, inner)
                                    | ExprVariant::Bracket(inner)
                                    | ExprVariant::Default(inner, _)
                                    | ExprVariant::Shift(inner, _) => find_any_format(inner),
                                    ExprVariant::Ite(a, b, c) => find_any_format(a)
                                        .or_else(|| find_any_format(b))
                                        .or_else(|| find_any_format(c)),
                                    ExprVariant::Tuple(el) => {
                                        for e in el.iter() {
                                            if let Some(s) = find_any_format(e) {
                                                return Some(s);
                                            }
                                        }
                                        None
                                    }
                                    ExprVariant::Field(inner, _) => find_any_format(inner),
                                    ExprVariant::SignalAccess(inner, _) => find_any_format(inner),
                                    ExprVariant::Quantified(_, _, _, body) => find_any_format(body),
                                    ExprVariant::Literal(_)
                                    | ExprVariant::MissingExpr
                                    | ExprVariant::Identifier(_) => None,
                                }
                            }

                            // Recursive search: returns true if any Quantified with Forall is found
                            fn expr_contains_forall(expr: &crate::ast::ExprNode) -> bool {
                                use crate::ast::ExprVariant;
                                match &expr.kind {
                                    ExprVariant::Quantified(q, _, _, _) => {
                                        use crate::ast::Quantifier;
                                        matches!(q, Quantifier::Forall)
                                    }
                                    ExprVariant::Method(_, _, _, args)
                                    | ExprVariant::Function(_, _, args) => {
                                        args.iter().any(|a| expr_contains_forall(a))
                                    }
                                    ExprVariant::Binary(_, l, r) => {
                                        expr_contains_forall(l) || expr_contains_forall(r)
                                    }
                                    ExprVariant::Unary(_, inner)
                                    | ExprVariant::Bracket(inner)
                                    | ExprVariant::Default(inner, _)
                                    | ExprVariant::Shift(inner, _) => expr_contains_forall(inner),
                                    ExprVariant::Ite(a, b, c) => {
                                        expr_contains_forall(a)
                                            || expr_contains_forall(b)
                                            || expr_contains_forall(c)
                                    }
                                    ExprVariant::Tuple(el) => {
                                        el.iter().any(|e| expr_contains_forall(e))
                                    }
                                    ExprVariant::Field(inner, _) => expr_contains_forall(inner),
                                    ExprVariant::SignalAccess(inner, _) => {
                                        expr_contains_forall(inner)
                                    }
                                    ExprVariant::Literal(_)
                                    | ExprVariant::MissingExpr
                                    | ExprVariant::Identifier(_) => false,
                                }
                            }

                            // Iterate over newly added constrains and their eval expressions
                            let new_len = self.ast.constrains.len();
                            for idx in start_len..new_len {
                                // clone constrain so we can modify and write back if needed
                                let c = self.ast.constrains[idx].as_ref().clone();
                                let mut new_c = c.clone();

                                // helper: replace Identifier args in format() with Default(SignalAccess(...).hold(), 0)
                                fn replace_format_args_in_expr(
                                    parser: &mut OORVSpecParser,
                                    e: &crate::ast::ExprNode,
                                    binds1: &Vec<crate::ast::Identifier>,
                                    binds2: &Vec<crate::ast::Identifier>,
                                ) -> crate::ast::ExprNode {
                                    use crate::ast::AccessMode;
                                    use crate::ast::ExprVariant;
                                    match &e.kind {
                                        ExprVariant::Method(inner, fname, types, args) => {
                                            // recurse into receiver
                                            let new_inner = Box::new(replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            ));
                                            // if this is a format call, rewrite its positional args referencing bindings
                                            if fname.name.name.as_str() == "format" {
                                                let mut new_args: Vec<crate::ast::ExprNode> =
                                                    Vec::new();
                                                if !args.is_empty() {
                                                    for a in args.iter() {
                                                        match &a.kind {
                                                            ExprVariant::Identifier(ident) => {
                                                                // try to match this ident to an entry in binds1 and map to binds2 by index
                                                                let mut mapped_name: Option<
                                                                    String,
                                                                > = None;
                                                                for (i, b) in
                                                                    binds1.iter().enumerate()
                                                                {
                                                                    // various matching styles: prefix, exact, suffix, contains
                                                                    let prefix =
                                                                        format!("{}::", b.name);
                                                                    if ident
                                                                        .name
                                                                        .starts_with(&prefix)
                                                                    {
                                                                        let rest = &ident.name
                                                                            [prefix.len()..];
                                                                        if let Some(b2) =
                                                                            binds2.get(i)
                                                                        {
                                                                            mapped_name =
                                                                                Some(format!(
                                                                                    "{}::{}",
                                                                                    b2.name, rest
                                                                                ));
                                                                        } else {
                                                                            mapped_name = Some(
                                                                                ident.name.clone(),
                                                                            );
                                                                        }
                                                                        break;
                                                                    }
                                                                    if ident.name == b.name {
                                                                        if let Some(b2) =
                                                                            binds2.get(i)
                                                                        {
                                                                            mapped_name = Some(
                                                                                b2.name.clone(),
                                                                            );
                                                                        } else {
                                                                            mapped_name = Some(
                                                                                ident.name.clone(),
                                                                            );
                                                                        }
                                                                        break;
                                                                    }
                                                                    if ident.name.ends_with(
                                                                        &format!("::{}", b.name),
                                                                    ) {
                                                                        // keep leading part and replace trailing b with b2
                                                                        if let Some(pos) = ident
                                                                            .name
                                                                            .rfind(&format!(
                                                                                "::{}",
                                                                                b.name
                                                                            ))
                                                                        {
                                                                            let lead =
                                                                                &ident.name[..pos];
                                                                            if let Some(b2) =
                                                                                binds2.get(i)
                                                                            {
                                                                                mapped_name =
                                                                                    Some(format!(
                                                                                        "{}::{}",
                                                                                        lead,
                                                                                        b2.name
                                                                                    ));
                                                                            } else {
                                                                                mapped_name = Some(
                                                                                    ident
                                                                                        .name
                                                                                        .clone(),
                                                                                );
                                                                            }
                                                                            break;
                                                                        }
                                                                    }
                                                                    if ident.name.contains(
                                                                        &format!("::{}::", b.name),
                                                                    ) {
                                                                        // replace the middle occurrence
                                                                        mapped_name = Some(ident.name.replace(&format!("::{}::", b.name), &format!("::{}::", binds2.get(i).map(|x| x.name.clone()).unwrap_or(b.name.clone()))));
                                                                        break;
                                                                    }
                                                                }
                                                                if let Some(new_name) = mapped_name
                                                                {
                                                                    // build Identifier expression with mapped name
                                                                    let new_ident =
                                                                        crate::ast::Identifier {
                                                                            name: new_name,
                                                                            span: a.span.clone(),
                                                                        };
                                                                    let inner_ident = crate::ast::ExprNode { kind: ExprVariant::Identifier(new_ident), node_id: parser.ast.alloc_node_id(), span: a.span.clone() };
                                                                    // SignalAccess(..., Hold)
                                                                    let stream = crate::ast::ExprNode { kind: ExprVariant::SignalAccess(Box::new(inner_ident), AccessMode::Cached), node_id: parser.ast.alloc_node_id(), span: a.span.clone() };
                                                                    // literal 0
                                                                    let lit = crate::ast::TokenLiteral { kind: crate::ast::LiteralKind::Number("0".to_string(), None), node_id: parser.ast.alloc_node_id(), span: a.span.clone() };
                                                                    let lit_expr =
                                                                        crate::ast::ExprNode {
                                                                            kind:
                                                                                ExprVariant::Literal(
                                                                                    lit,
                                                                                ),
                                                                            node_id: parser
                                                                                .ast
                                                                                .alloc_node_id(),
                                                                            span: a.span.clone(),
                                                                        };
                                                                    // Default(stream, 0)
                                                                    let def =
                                                                        crate::ast::ExprNode {
                                                                            kind:
                                                                                ExprVariant::Default(
                                                                                    Box::new(
                                                                                        stream,
                                                                                    ),
                                                                                    Box::new(
                                                                                        lit_expr,
                                                                                    ),
                                                                                ),
                                                                            node_id: parser
                                                                                .ast
                                                                                .alloc_node_id(),
                                                                            span: a.span.clone(),
                                                                        };
                                                                    new_args.push(def);
                                                                } else {
                                                                    new_args.push(a.clone());
                                                                }
                                                            }
                                                            _ => {
                                                                new_args.push(
                                                                    replace_format_args_in_expr(
                                                                        parser, a, binds1, binds2,
                                                                    ),
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                return crate::ast::ExprNode {
                                                    kind: ExprVariant::Method(
                                                        new_inner,
                                                        fname.clone(),
                                                        types.clone(),
                                                        new_args,
                                                    ),
                                                    node_id: parser.ast.alloc_node_id(),
                                                    span: e.span.clone(),
                                                };
                                            }
                                            // not format: recurse into args normally
                                            let nargs: Vec<crate::ast::ExprNode> = args
                                                .iter()
                                                .map(|a| {
                                                    replace_format_args_in_expr(
                                                        parser, a, binds1, binds2,
                                                    )
                                                })
                                                .collect();
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Method(
                                                    new_inner,
                                                    fname.clone(),
                                                    types.clone(),
                                                    nargs,
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Function(name, types, args) => {
                                            let nargs: Vec<crate::ast::ExprNode> = args
                                                .iter()
                                                .map(|a| {
                                                    replace_format_args_in_expr(
                                                        parser, a, binds1, binds2,
                                                    )
                                                })
                                                .collect();
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Function(
                                                    name.clone(),
                                                    types.clone(),
                                                    nargs,
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Binary(op, l, r) => {
                                            let nl = replace_format_args_in_expr(
                                                parser, l, binds1, binds2,
                                            );
                                            let nr = replace_format_args_in_expr(
                                                parser, r, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Binary(
                                                    *op,
                                                    Box::new(nl),
                                                    Box::new(nr),
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Unary(op, inner) => {
                                            let ni = replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Unary(*op, Box::new(ni)),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Bracket(inner) => {
                                            let ni = replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Bracket(Box::new(ni)),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Default(inner, def) => {
                                            let ni = replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            );
                                            let nd = replace_format_args_in_expr(
                                                parser, def, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Default(
                                                    Box::new(ni),
                                                    Box::new(nd),
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Ite(a, b, c) => {
                                            let na = replace_format_args_in_expr(
                                                parser, a, binds1, binds2,
                                            );
                                            let nb = replace_format_args_in_expr(
                                                parser, b, binds1, binds2,
                                            );
                                            let nc = replace_format_args_in_expr(
                                                parser, c, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Ite(
                                                    Box::new(na),
                                                    Box::new(nb),
                                                    Box::new(nc),
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Tuple(el) => {
                                            let nel: Vec<crate::ast::ExprNode> = el
                                                .iter()
                                                .map(|x| {
                                                    replace_format_args_in_expr(
                                                        parser, x, binds1, binds2,
                                                    )
                                                })
                                                .collect();
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Tuple(nel),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::Field(inner, id) => {
                                            let ni = replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::Field(Box::new(ni), id.clone()),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        ExprVariant::SignalAccess(inner, kind) => {
                                            let ni = replace_format_args_in_expr(
                                                parser, inner, binds1, binds2,
                                            );
                                            crate::ast::ExprNode {
                                                kind: ExprVariant::SignalAccess(
                                                    Box::new(ni),
                                                    *kind,
                                                ),
                                                node_id: parser.ast.alloc_node_id(),
                                                span: e.span.clone(),
                                            }
                                        }
                                        // leave other leaf nodes unchanged (catch-all)
                                        _ => e.clone(),
                                    }
                                }

                                // run existing checks and also apply transforms when condition is Quantified
                                for ev in new_c.eval.iter_mut() {
                                    // If condition is Quantified, first rewrite eval_expression format args
                                    if let Some(cond) = &ev.condition {
                                        if let crate::ast::ExprVariant::Quantified(
                                            _q,
                                            binds1,
                                            binds2,
                                            _body,
                                        ) = &cond.kind
                                        {
                                            // Build deduplicated binds1/binds2 preserving first occurrence
                                            use std::collections::HashSet;
                                            let mut seen_names: HashSet<String> = HashSet::new();
                                            let mut uniq_b1: Vec<crate::ast::Identifier> =
                                                Vec::new();
                                            let mut uniq_b2: Vec<crate::ast::Identifier> =
                                                Vec::new();
                                            for (i, b) in binds1.iter().enumerate() {
                                                if !seen_names.contains(&b.name) {
                                                    seen_names.insert(b.name.clone());
                                                    uniq_b1.push(b.clone());
                                                    if let Some(b2) = binds2.get(i) {
                                                        uniq_b2.push(b2.clone());
                                                    } else {
                                                        // fallback: duplicate b as placeholder
                                                        uniq_b2.push(b.clone());
                                                    }
                                                }
                                            }

                                            // if eval_expression contains format, first validate ordering then rewrite its format args
                                            if let Some(e2) = &ev.expression {
                                                // helper: collect the order of bind names that appear as Idents inside format(...) args
                                                fn collect_binds_in_format_args(
                                                    expr: &crate::ast::ExprNode,
                                                    binds_list: &Vec<crate::ast::Identifier>,
                                                ) -> Vec<String>
                                                {
                                                    use crate::ast::ExprVariant;
                                                    let mut res: Vec<String> = Vec::new();
                                                    match &expr.kind {
                                                        ExprVariant::Method(_, fname, _, args) => {
                                                            if fname.name.name.as_str() == "format"
                                                            {
                                                                for a in args.iter() {
                                                                    match &a.kind {
                                                                        ExprVariant::Identifier(
                                                                            id,
                                                                        ) => {
                                                                            for b in
                                                                                binds_list.iter()
                                                                            {
                                                                                // match several styles
                                                                                let prefix = format!(
                                                                                    "{}::",
                                                                                    b.name
                                                                                );
                                                                                if id.name.starts_with(&prefix) || id.name == b.name || id.name.ends_with(&format!("::{}", b.name)) || id.name.contains(&format!("::{}::", b.name)) {
                                                                                    if !res.contains(&b.name) {
                                                                                        res.push(b.name.clone());
                                                                                    }
                                                                                    break;
                                                                                }
                                                                            }
                                                                        }
                                                                        _ => {}
                                                                    }
                                                                }
                                                            }
                                                            // continue searching into args/receiver
                                                            for a in args.iter() {
                                                                let mut more =
                                                                    collect_binds_in_format_args(
                                                                        a, binds_list,
                                                                    );
                                                                res.append(&mut more);
                                                            }
                                                        }
                                                        ExprVariant::Function(_, _, args) => {
                                                            for a in args.iter() {
                                                                let mut more =
                                                                    collect_binds_in_format_args(
                                                                        a, binds_list,
                                                                    );
                                                                res.append(&mut more);
                                                            }
                                                        }
                                                        ExprVariant::Binary(_, l, r) => {
                                                            let mut lres =
                                                                collect_binds_in_format_args(
                                                                    l, binds_list,
                                                                );
                                                            res.append(&mut lres);
                                                            let mut rres =
                                                                collect_binds_in_format_args(
                                                                    r, binds_list,
                                                                );
                                                            res.append(&mut rres);
                                                        }
                                                        ExprVariant::Unary(_, inner)
                                                        | ExprVariant::Bracket(inner)
                                                        | ExprVariant::Default(inner, _)
                                                        | ExprVariant::Shift(inner, _) => {
                                                            let mut inner_res =
                                                                collect_binds_in_format_args(
                                                                    inner, binds_list,
                                                                );
                                                            res.append(&mut inner_res);
                                                        }
                                                        ExprVariant::Ite(a, b, c) => {
                                                            let mut ares =
                                                                collect_binds_in_format_args(
                                                                    a, binds_list,
                                                                );
                                                            res.append(&mut ares);
                                                            let mut bres =
                                                                collect_binds_in_format_args(
                                                                    b, binds_list,
                                                                );
                                                            res.append(&mut bres);
                                                            let mut cres =
                                                                collect_binds_in_format_args(
                                                                    c, binds_list,
                                                                );
                                                            res.append(&mut cres);
                                                        }
                                                        ExprVariant::Tuple(el) => {
                                                            for e in el.iter() {
                                                                let mut more =
                                                                    collect_binds_in_format_args(
                                                                        e, binds_list,
                                                                    );
                                                                res.append(&mut more);
                                                            }
                                                        }
                                                        ExprVariant::Field(inner, _) => {
                                                            let mut more =
                                                                collect_binds_in_format_args(
                                                                    inner, binds_list,
                                                                );
                                                            res.append(&mut more);
                                                        }
                                                        ExprVariant::SignalAccess(inner, _) => {
                                                            let mut more =
                                                                collect_binds_in_format_args(
                                                                    inner, binds_list,
                                                                );
                                                            res.append(&mut more);
                                                        }
                                                        ExprVariant::Quantified(_, _, _, body) => {
                                                            let mut more =
                                                                collect_binds_in_format_args(
                                                                    body, binds_list,
                                                                );
                                                            res.append(&mut more);
                                                        }
                                                        _ => {}
                                                    }
                                                    // dedupe preserving order
                                                    let mut seen: HashSet<String> = HashSet::new();
                                                    let mut dedup: Vec<String> = Vec::new();
                                                    for n in res.into_iter() {
                                                        if !seen.contains(&n) {
                                                            seen.insert(n.clone());
                                                            dedup.push(n);
                                                        }
                                                    }
                                                    dedup
                                                }

                                                let used_order =
                                                    collect_binds_in_format_args(e2, &uniq_b1);
                                                // compare used_order with uniq_b1 names
                                                let expected: Vec<String> = uniq_b1
                                                    .iter()
                                                    .map(|x| x.name.clone())
                                                    .collect();
                                                if !used_order.is_empty() && used_order != expected
                                                {
                                                    let span_for_diag = e2.span.clone();
                                                    if first_error.is_none() {
                                                        first_error = Some(oorv_error_with_span("`format` arguments must follow the quantified binding order", Some(span_for_diag)));
                                                    }
                                                }

                                                // perform rewrite using deduplicated binds
                                                let replaced = replace_format_args_in_expr(
                                                    self, e2, &uniq_b1, &uniq_b2,
                                                );
                                                ev.expression = Some(replaced);
                                            }
                                            // also forbid format under forall explicitly (previous diagnostic)
                                            if expr_contains_forall(cond) {
                                                if let Some(e2) = &ev.expression {
                                                    if let Some(span2) = find_any_format(e2) {
                                                        if first_error.is_none() {
                                                            first_error = Some(oorv_error_with_span("`format` is not allowed in `forall` clauses", Some(span2)));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // After potential rewriting, check that any format in WorldDecl references a uid SignalAccess
                                    if let Some(e) = &ev.expression {
                                        if let Some(span) = find_format_without_uid(e) {
                                            if first_error.is_none() {
                                                first_error = Some(oorv_error_with_span("`format` in WorldDecl must reference a `uid` SignalAccess parameter", Some(span)));
                                            }
                                        }
                                    }
                                }

                                // write back possibly modified constrain
                                self.ast.constrains[idx] = std::rc::Rc::new(new_c);
                            }
                        }
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(())
    }

    fn parse_fun_decl(
        &mut self,
        pair: Pair<'_, Rule>,
        module_name: Option<Identifier>,
    ) -> Result<crate::ast::GlobalMethodDecl, OORVError> {
        // lightweight parsing of `Fun` declarations; reuse global method AST
        assert_eq!(pair.as_rule(), Rule::FuncDef);
        let span_span = pair.as_span();
        let span = SourceSpan::Direct {
            start: span_span.start(),
            end: span_span.end(),
        };
        let mut inner = pair.into_inner();
        let name_pair = inner.next().expect("Expected Identifier in FunDecl");
        let name = self.extract_ident(&name_pair);

        let mut params = Vec::new();
        let mut return_type: Option<crate::ast::ValueType> = None;
        let mut body: Option<crate::ast::MethodBody> = None;

        for p in inner {
            match p.as_rule() {
                Rule::ParamList => params = self.gather_parameters(p.into_inner()),
                Rule::TypeRef => return_type = Some(self.resolve_type(p)),
                Rule::FuncBody => {
                    // FunctionBody now yields structured pairs: `LetDecl` and `ExprStmt` and an optional trailing `Expr` as return.
                    let span_fb_span = p.as_span();
                    let span_fb: SourceSpan = SourceSpan::Direct {
                        start: span_fb_span.start(),
                        end: span_fb_span.end(),
                    };
                    let mut stmts: Vec<crate::ast::MethodStmt> = Vec::new();
                    let mut ret: Option<ExprNode> = None;

                    for child in p.into_inner() {
                        match child.as_rule() {
                            Rule::LetStmt => {
                                // LetDecl -> LetStart, LetEnd
                                // capture the span of the let declaration before moving `child`
                                let let_span_span = child.as_span();
                                let let_span: SourceSpan = SourceSpan::Direct {
                                    start: let_span_span.start(),
                                    end: let_span_span.end(),
                                };
                                let mut let_inner = child.into_inner();
                                let start = let_inner.next().expect("LetDecl must have LetStart");
                                let end = let_inner.next().expect("LetDecl must have LetEnd");
                                // LetStart contains Identifier
                                let name_pair = start
                                    .into_inner()
                                    .next()
                                    .expect("LetStart must contain Identifier");
                                let name = self.extract_ident(&name_pair);
                                // LetEnd contains '=' Expr ';' -> inner Expr is the expression
                                let expr_pair =
                                    end.into_inner().next().expect("LetEnd must contain Expr");
                                match self.construct_expr_node(expr_pair.into_inner(), None, None) {
                                    Ok(expr) => {
                                        let let_decl = crate::ast::LetDecl {
                                            name,
                                            expr,
                                            span: let_span.clone(),
                                        };
                                        stmts.push(crate::ast::MethodStmt::Let(let_decl));
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            Rule::AssignStmt => {
                                let let_span_span = child.as_span();
                                let let_span: SourceSpan = SourceSpan::Direct {
                                    start: let_span_span.start(),
                                    end: let_span_span.end(),
                                };
                                let mut let_inner = child.into_inner();

                                // Parse the variable name.
                                let name_pair =
                                    let_inner.next().expect("LetStart must contain Identifier");
                                let name = self.extract_ident(&name_pair);

                                // Extract the LetValue pair.
                                let let_end_pair =
                                    let_inner.next().expect("LetEnd must contain expression");

                                // Extract the Expr node from LetValue.
                                let mut expr_inner = let_end_pair.into_inner();

                                let expr_pair =
                                    expr_inner.next().expect("LetEnd must contain Expr");

                                // Descend into Expr pairs before passing to construct_expr_node.
                                match self.construct_expr_node(expr_pair.into_inner(), None, None) {
                                    Ok(expr) => {
                                        let let_decl = crate::ast::LetDecl {
                                            name,
                                            expr,
                                            span: let_span.clone(),
                                        };
                                        stmts.push(crate::ast::MethodStmt::Let(let_decl));
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            Rule::Expr => {
                                // trailing return expression
                                match self.construct_expr_node(child.into_inner(), None, None) {
                                    Ok(e) => ret = Some(e),
                                    Err(e) => return Err(e),
                                }
                            }
                            _ => {}
                        }
                    }

                    let mb = crate::ast::MethodBody {
                        decls: stmts,
                        ret,
                        span: span_fb,
                    };
                    body = Some(mb);
                }
                _ => {}
            }
        }

        let body = body.unwrap_or(crate::ast::MethodBody {
            decls: Vec::new(),
            ret: None,
            span,
        });

        Ok(GlobalMethodDecl {
            node_id: self.ast.alloc_node_id(),
            name,
            module_name: module_name.clone(),
            params: params.into_iter().map(Rc::new).collect(),
            return_type,
            body,
            span,
        })
    }

    fn resolve_type(&self, pair: Pair<'_, Rule>) -> ValueType {
        debug_assert_eq!(pair.as_rule(), Rule::TypeRef);
        let outer_span = pair.as_span();
        let elems: Vec<ValueType> = Vec::new();

        for child in pair.into_inner() {
            match child.as_rule() {
                Rule::Ident => {
                    let name = child.as_str().to_owned();
                    return ValueType {
                        kind: ValueTypeKind::Named(name),
                        node_id: self.ast.alloc_node_id(),
                        span: SourceSpan::Direct {
                            start: child.as_span().start(),
                            end: child.as_span().end(),
                        },
                    };
                }
                other => panic!(
                    "internal parser invariant violated: {:?} encountered in Type",
                    other
                ),
            }
        }

        ValueType {
            kind: ValueTypeKind::Tuple(elems),
            node_id: self.ast.alloc_node_id(),
            span: SourceSpan::Direct {
                start: outer_span.start(),
                end: outer_span.end(),
            },
        }
    }

    fn extract_ident(&self, p: &Pair<'_, Rule>) -> Identifier {
        debug_assert_eq!(p.as_rule(), Rule::Ident);
        let s = p.as_str().trim().to_string();
        Identifier {
            name: s,
            span: SourceSpan::Direct {
                start: p.as_span().start(),
                end: p.as_span().end(),
            },
        }
    }

    fn parse_value_literal(&self, pair: Pair<'_, Rule>) -> TokenLiteral {
        debug_assert!(matches!(pair.as_rule(), Rule::ValueLit | Rule::ConstLit));
        let child = pair
            .into_inner()
            .next()
            .expect("literal node expected to have a single child");

        match child.as_rule() {
            Rule::StrBody => {
                let s = child.as_str().to_string();
                TokenLiteral {
                    kind: LiteralKind::Text(s),
                    node_id: self.ast.alloc_node_id(),
                    span: SourceSpan::Direct {
                        start: child.as_span().start(),
                        end: child.as_span().end(),
                    },
                }
            }
            Rule::NumLit => self.parse_numeric_literal(child),
            Rule::BoolTrue => TokenLiteral {
                kind: LiteralKind::Boolean(true),
                node_id: self.ast.alloc_node_id(),
                span: SourceSpan::Direct {
                    start: child.as_span().start(),
                    end: child.as_span().end(),
                },
            },
            Rule::BoolFalse => TokenLiteral {
                kind: LiteralKind::Boolean(false),
                node_id: self.ast.alloc_node_id(),
                span: SourceSpan::Direct {
                    start: child.as_span().start(),
                    end: child.as_span().end(),
                },
            },
            other => panic!("unexpected literal kind: {:?}", other),
        }
    }

    fn parse_numeric_literal(&self, pair: Pair<'_, Rule>) -> TokenLiteral {
        debug_assert_eq!(pair.as_rule(), Rule::NumLit);
        let span = pair.as_span();
        let mut it = pair.into_inner();
        let num_tok = it
            .next()
            .expect("expected numeric token inside NumberLiteral");
        let name = num_tok.as_str().to_string();
        let unit = it.next().map(|u| u.as_str().to_string());

        TokenLiteral {
            kind: LiteralKind::Number(name, unit),
            node_id: self.ast.alloc_node_id(),
            span: SourceSpan::Direct {
                start: span.start(),
                end: span.end(),
            },
        }
    }

    // Helper: union two SourceSpan values into a single span covering both.
    fn union_span(
        a: &crate::ast::SourceSpan,
        b: &crate::ast::SourceSpan,
    ) -> crate::ast::SourceSpan {
        use crate::ast::SourceSpan;
        match (a, b) {
            (
                SourceSpan::Direct { start: sa, end: ea },
                SourceSpan::Direct { start: sb, end: eb },
            ) => {
                let start = std::cmp::min(*sa, *sb);
                let end = std::cmp::max(*ea, *eb);
                SourceSpan::Direct { start, end }
            }
            (
                SourceSpan::Direct { start: sa, end: ea },
                SourceSpan::Indirect { start: sb, end: eb },
            )
            | (
                SourceSpan::Indirect { start: sb, end: eb },
                SourceSpan::Direct { start: sa, end: ea },
            )
            | (
                SourceSpan::Indirect { start: sa, end: ea },
                SourceSpan::Indirect { start: sb, end: eb },
            ) => {
                let start = std::cmp::min(*sa, *sb);
                let end = std::cmp::max(*ea, *eb);
                SourceSpan::Indirect { start, end }
            }
            (SourceSpan::Unknown, other) | (other, SourceSpan::Unknown) => other.clone(),
        }
    }

    fn construct_expr_node(
        &self,
        pairs: Pairs<'_, Rule>,
        module_name: Option<Identifier>,
        class_name: Option<Identifier>,
    ) -> Result<ExprNode, OORVError> {
        pratt_parser()
            .map_primary(|primary| {
                // build primary expression then apply replacement so this also runs for non-infix expressions
                let expr = self.construct_term_node(primary)?;
                let mut out = expr.clone();
                // If we have module+class context, perform a recursive `self::` replacement
                // for the whole expression tree so tuples and other top-level forms
                // are handled uniformly.
                if let (Some(m), Some(c)) = (&module_name, &class_name) {
                    fn replace_self_in_expr(e: &ExprNode, m: &crate::ast::Identifier, c: &crate::ast::Identifier) -> ExprNode {
                        match &e.kind {
                            ExprVariant::Identifier(id) => {
                                        if id.name.starts_with("self::") {
                                            let rest = id.name.trim_start_matches("self::");
                                            let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                            return ExprNode { kind: ExprVariant::Identifier(crate::ast::Identifier { name: new_name, span: id.span.clone() }), node_id: e.node_id, span: e.span };
                                        }
                                        e.clone()
                            }
                            ExprVariant::Literal(lit) => {
                                if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                                    if s.contains("self::") {
                                        let replaced = s.replace("self::", &format!("{}::{}::", m.name, c.name));
                                        let new_lit = TokenLiteral { kind: LiteralKind::Text(replaced), node_id: lit.node_id, span: lit.span.clone() };
                                        return ExprNode { kind: ExprVariant::Literal(new_lit), node_id: e.node_id, span: e.span };
                                    }
                                }
                                e.clone()
                            }
                            ExprVariant::SignalAccess(inner, kind) => {
                                let ni = replace_self_in_expr(inner, m, c);
                                ExprNode { kind: ExprVariant::SignalAccess(Box::new(ni), *kind), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Default(expr0, default) => {
                                let ne = replace_self_in_expr(expr0, m, c);
                                let nd = replace_self_in_expr(default, m, c);
                                ExprNode { kind: ExprVariant::Default(Box::new(ne), Box::new(nd)), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Shift(expr0, off) => {
                                let ne = replace_self_in_expr(expr0, m, c);
                                ExprNode { kind: ExprVariant::Shift(Box::new(ne), *off), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Binary(op, l, r) => {
                                let nl = replace_self_in_expr(l, m, c);
                                let nr = replace_self_in_expr(r, m, c);
                                ExprNode { kind: ExprVariant::Binary(*op, Box::new(nl), Box::new(nr)), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Unary(op, inner) => {
                                let ni = replace_self_in_expr(inner, m, c);
                                ExprNode { kind: ExprVariant::Unary(*op, Box::new(ni)), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Ite(cond, cons, alt) => {
                                let nc = replace_self_in_expr(cond, m, c);
                                let nn = replace_self_in_expr(cons, m, c);
                                let na = replace_self_in_expr(alt, m, c);
                                ExprNode { kind: ExprVariant::Ite(Box::new(nc), Box::new(nn), Box::new(na)), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Bracket(inner) => {
                                let ni = replace_self_in_expr(inner, m, c);
                                ExprNode { kind: ExprVariant::Bracket(Box::new(ni)), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Tuple(entries) => {
                                let ne: Vec<ExprNode> = entries.iter().map(|en| replace_self_in_expr(en, m, c)).collect();
                                ExprNode { kind: ExprVariant::Tuple(ne), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Field(inner, id) => {
                                let ni = replace_self_in_expr(inner, m, c);
                                ExprNode { kind: ExprVariant::Field(Box::new(ni), id.clone()), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Method(inner, fname, types, args) => {
                                let ni = replace_self_in_expr(inner, m, c);
                                let nargs: Vec<ExprNode> = args.iter().map(|a| replace_self_in_expr(a, m, c)).collect();
                                ExprNode { kind: ExprVariant::Method(Box::new(ni), fname.clone(), types.clone(), nargs), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Function(fname, types, fargs) => {
                                let nargs: Vec<ExprNode> = fargs.iter().map(|a| replace_self_in_expr(a, m, c)).collect();
                                ExprNode { kind: ExprVariant::Function(fname.clone(), types.clone(), nargs), node_id: e.node_id, span: e.span }
                            }
                            ExprVariant::Quantified(q, binds1, binds2, body) => {
                                let nbody = replace_self_in_expr(body, m, c);
                                ExprNode { kind: ExprVariant::Quantified(q.clone(), binds1.clone(), binds2.clone(), Box::new(nbody)), node_id: e.node_id, span: e.span }
                            }
                            _ => e.clone(),
                        }
                    }

                    out = replace_self_in_expr(&expr, m, c);
                }

                match &expr.kind {
                    ExprVariant::Identifier(id) => {
                        if id.name.starts_with("self::") {
                            if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                let rest = id.name.trim_start_matches("self::");
                                let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                out = ExprNode { kind: ExprVariant::Identifier(crate::ast::Identifier { name: new_name, span: id.span.clone() }), node_id: expr.node_id, span: expr.span };
                            }
                        }
                    }
                    ExprVariant::Literal(lit) => {
                        if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                            if s.contains("self::") {
                                if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                    let replaced = s.replace("self::", &format!("{}::{}::", m.name, c.name));
                                    let new_lit = TokenLiteral { kind: LiteralKind::Text(replaced), node_id: lit.node_id, span: lit.span.clone() };
                                    out = ExprNode { kind: ExprVariant::Literal(new_lit), node_id: expr.node_id, span: expr.span };
                                }
                            }
                        }
                    }
                    ExprVariant::Function(name, types, args) => {
                        if let (Some(m), Some(c)) = (&module_name, &class_name) {
                            // Recursive replacer: walk expression tree and replace `self::` occurrences
                            fn replace_self_in_expr(e: &ExprNode, m: &crate::ast::Identifier, c: &crate::ast::Identifier) -> ExprNode {
                                match &e.kind {
                                    ExprVariant::Identifier(id) => {
                                        if id.name.starts_with("self::") {
                                            let rest = id.name.trim_start_matches("self::");
                                            let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                            return ExprNode { kind: ExprVariant::Identifier(crate::ast::Identifier { name: new_name, span: id.span.clone() }), node_id: e.node_id, span: e.span };
                                        }
                                        e.clone()
                                    }
                                    ExprVariant::Literal(lit) => {
                                        if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                                            if s.contains("self::") {
                                                let replaced = s.replace("self::", &format!("{}::{}::", m.name, c.name));
                                                let new_lit = crate::ast::TokenLiteral { kind: LiteralKind::Text(replaced), node_id: lit.node_id, span: lit.span.clone() };
                                                return ExprNode { kind: ExprVariant::Literal(new_lit), node_id: e.node_id, span: e.span };
                                            }
                                        }
                                        e.clone()
                                    }
                                    ExprVariant::SignalAccess(inner, kind) => {
                                        let ni = replace_self_in_expr(inner, m, c);
                                        ExprNode { kind: ExprVariant::SignalAccess(Box::new(ni), *kind), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Default(expr0, default) => {
                                        let ne = replace_self_in_expr(expr0, m, c);
                                        let nd = replace_self_in_expr(default, m, c);
                                        ExprNode { kind: ExprVariant::Default(Box::new(ne), Box::new(nd)), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Shift(expr0, off) => {
                                        let ne = replace_self_in_expr(expr0, m, c);
                                        ExprNode { kind: ExprVariant::Shift(Box::new(ne), *off), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Binary(op, l, r) => {
                                        let nl = replace_self_in_expr(l, m, c);
                                        let nr = replace_self_in_expr(r, m, c);
                                        ExprNode { kind: ExprVariant::Binary(*op, Box::new(nl), Box::new(nr)), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Unary(op, inner) => {
                                        let ni = replace_self_in_expr(inner, m, c);
                                        ExprNode { kind: ExprVariant::Unary(*op, Box::new(ni)), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Ite(cond, cons, alt) => {
                                        let nc = replace_self_in_expr(cond, m, c);
                                        let nn = replace_self_in_expr(cons, m, c);
                                        let na = replace_self_in_expr(alt, m, c);
                                        ExprNode { kind: ExprVariant::Ite(Box::new(nc), Box::new(nn), Box::new(na)), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Bracket(inner) => {
                                        let ni = replace_self_in_expr(inner, m, c);
                                        ExprNode { kind: ExprVariant::Bracket(Box::new(ni)), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Tuple(entries) => {
                                        let ne: Vec<ExprNode> = entries.iter().map(|en| replace_self_in_expr(en, m, c)).collect();
                                        ExprNode { kind: ExprVariant::Tuple(ne), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Field(inner, id) => {
                                        let ni = replace_self_in_expr(inner, m, c);
                                        ExprNode { kind: ExprVariant::Field(Box::new(ni), id.clone()), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Method(inner, fname, types, args) => {
                                        let ni = replace_self_in_expr(inner, m, c);
                                        let nargs: Vec<ExprNode> = args.iter().map(|a| replace_self_in_expr(a, m, c)).collect();
                                        ExprNode { kind: ExprVariant::Method(Box::new(ni), fname.clone(), types.clone(), nargs), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Function(fname, types, fargs) => {
                                        let nargs: Vec<ExprNode> = fargs.iter().map(|a| replace_self_in_expr(a, m, c)).collect();
                                        ExprNode { kind: ExprVariant::Function(fname.clone(), types.clone(), nargs), node_id: e.node_id, span: e.span }
                                    }
                                    ExprVariant::Quantified(q, binds1, binds2, body) => {
                                        let nbody = replace_self_in_expr(body, m, c);
                                        ExprNode { kind: ExprVariant::Quantified(q.clone(), binds1.clone(), binds2.clone(), Box::new(nbody)), node_id: e.node_id, span: e.span }
                                    }
                                    _ => e.clone(),
                                }
                            }

                            let mut new_args: Vec<ExprNode> = Vec::new();
                            for a in args.iter() {
                                let a_out = replace_self_in_expr(a, m, c);
                                new_args.push(a_out);
                            }
                            out = ExprNode { kind: ExprVariant::Function(name.clone(), types.clone(), new_args), node_id: expr.node_id, span: expr.span };
                        }
                    }
                    _ => {}
                }
                Ok(out)
            })
            .map_infix(|lhs, op, rhs| {

                // Reduce function combining `ExprNode`s to `ExprNode`s with the correct precs
                let lhs = match lhs {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                let rhs = match rhs {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };

                // inline replacement on both sides
                let mut lhs_out = lhs.clone();
                match &lhs.kind {
                    ExprVariant::Identifier(id) => {
                        if id.name.starts_with("self::") {
                                        if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                            let rest = id.name.trim_start_matches("self::");
                                            let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                            lhs_out = ExprNode { kind: ExprVariant::Identifier(crate::ast::Identifier { name: new_name, span: id.span.clone() }), node_id: lhs.node_id, span: lhs.span };
                                        }
                        }
                    }
                    ExprVariant::Literal(lit) => {
                                        if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                                            if s.contains("self::") {
                                                if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                                    let replaced = s.replace("self::", &format!("{}::{}::", m.name, c.name));
                                                    let new_lit = crate::ast::TokenLiteral { kind: LiteralKind::Text(replaced), node_id: lit.node_id, span: lit.span.clone() };
                                                    lhs_out = ExprNode { kind: ExprVariant::Literal(new_lit), node_id: lhs.node_id, span: lhs.span };
                                                }
                                            }
                                        }
                    }
                    _ => {}
                }

                let mut rhs_out = rhs.clone();
                match &rhs.kind {
                    ExprVariant::Identifier(id) => {
                                if id.name.starts_with("self::") {
                                    if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                        let rest = id.name.trim_start_matches("self::");
                                        let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                        rhs_out = ExprNode { kind: ExprVariant::Identifier(crate::ast::Identifier { name: new_name, span: id.span.clone() }), node_id: rhs.node_id, span: rhs.span };
                                    }
                                }
                    }
                    ExprVariant::Literal(lit) => {
                        if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                            if s.contains("self::") {
                                if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                    let replaced = s.replace("self::", &format!("{}::{}::", m.name, c.name));
                                    let new_lit = crate::ast::TokenLiteral { kind: LiteralKind::Text(replaced), node_id: lit.node_id, span: lit.span.clone() };
                                    rhs_out = ExprNode { kind: ExprVariant::Literal(new_lit), node_id: rhs.node_id, span: rhs.span };
                                }
                            }
                        }
                    }
                    _ => {}
                };

                // shadow original lhs/rhs with possibly-updated versions
                let lhs = lhs_out;
                let rhs = rhs_out;
                let span = Self::union_span(&lhs.span, &rhs.span);
                let op = match op.as_rule() {
                    // Arithmetic
                    Rule::OpAdd => BinaryOp::Add,
                    Rule::OpSub => BinaryOp::Sub,
                    Rule::OpMul => BinaryOp::Mul,
                    Rule::OpDiv => BinaryOp::Div,
                    Rule::OpRem => BinaryOp::Rem,
                    Rule::OpPow => BinaryOp::Pow,
                    // Logical
                    Rule::OpAnd => BinaryOp::And,
                    Rule::OpOr => BinaryOp::Or,
                    // Comparison
                    Rule::CmpLt => BinaryOp::Lt,
                    Rule::CmpLe => BinaryOp::Le,
                    Rule::CmpGt => BinaryOp::Gt,
                    Rule::CmpGe => BinaryOp::Ge,
                    Rule::CmpEq => BinaryOp::Eq,
                    Rule::CmpNe => BinaryOp::Ne,
                    // Bitwise
                    Rule::OpBitAnd => BinaryOp::BitAnd,
                    Rule::OpBitOr => BinaryOp::BitOr,
                    Rule::OpBitXor => BinaryOp::BitXor,
                    Rule::OpShl => BinaryOp::Shl,
                    Rule::OpShr => BinaryOp::Shr,
                    // bubble up the unary operator on the lhs (if it exists) to fix precedence
                    Rule::OpDot => {
                        let (unop, binop_span, inner) = match lhs.kind {
                            ExprVariant::Unary(unop, inner) => (Some(unop), Self::union_span(&inner.span, &rhs.span), inner),
                            _ => (None, span, Box::new(lhs)),
                        };
                        match rhs.kind {
                            // access to a tuple
                            ExprVariant::Literal(l) => {
                                let ident = match l.kind {
                                    LiteralKind::Number(val, unit) => {
                                        assert!(unit.is_none());
                                        crate::ast::Identifier { name: val, span: l.span }
                                    }
                                    _ => {
                                        return Err(oorv_error_with_span(&format!("expected unsigned integer, found {:?}", l), Some(l.span.clone())));
                                    }
                                };
                                let binop_expr = ExprNode { kind: ExprVariant::Field(inner, ident), node_id: self.ast.alloc_node_id(), span: binop_span };
                                match unop {
                                    None => return Ok(binop_expr),
                                    Some(unop) => {
                                        return Ok(ExprNode { kind: ExprVariant::Unary(unop, Box::new(binop_expr)), node_id: self.ast.alloc_node_id(), span });
                                    }
                                }
                            }
                            // access to a named field: `obj.foo`
                            // Attempt to collapse chained Field/Identifier into a single Identifier with `::` separators,
                            // e.g. `self.a` -> `self::a`, `car.wheel.speed` -> `car::wheel::speed`.
                            ExprVariant::Identifier(i) => {
                                let field_ident = i.clone();

                                // helper: try to flatten the `inner` expression if it is a chain of Field/Identifier
                                fn flatten_ident_chain(expr: &ExprNode) -> Option<(String, SourceSpan)> {
                                    match &expr.kind {
                                        ExprVariant::Identifier(id) => {
                                            Some((id.name.clone(), id.span.clone()))
                                        }
                                        ExprVariant::Field(inner, id) => {
                                            if let Some((base, span)) = flatten_ident_chain(inner) {
                                                let combined = format!("{}::{}", base, id.name);
                                                let combined_span = OORVSpecParser::union_span(&span, &id.span);
                                                Some((combined, combined_span))
                                            } else {
                                                None
                                            }
                                        }
                                        _ => None,
                                    }
                                }

                                    if let Some((base_name, base_span)) = flatten_ident_chain(&inner) {
                                    // combine base chain with current field ident
                                    let combined_name = format!("{}::{}", base_name, field_ident.name);
                                    let combined_span = Self::union_span(&base_span, &field_ident.span);
                                    let new_ident = crate::ast::Identifier { name: combined_name, span: combined_span };
                                    // build Identifier expression; then if it starts with `self::` try to expand
                                        let mut new_expr = ExprNode { kind: ExprVariant::Identifier(new_ident.clone()), node_id: self.ast.alloc_node_id(), span: Self::union_span(&inner.span, &field_ident.span) };
                                    if new_ident.name.starts_with("self::") {
                                        if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                            let rest = new_ident.name.trim_start_matches("self::");
                                            let new_name = format!("{}::{}::{}", m.name, c.name, rest);
                                            let replaced_ident = crate::ast::Identifier { name: new_name, span: new_ident.span.clone() };
                                            new_expr = ExprNode { kind: ExprVariant::Identifier(replaced_ident), node_id: self.ast.alloc_node_id(), span: Self::union_span(&inner.span, &field_ident.span) };
                                        }
                                    }
                                    match unop {
                                        None => return Ok(new_expr),
                                        Some(unop) => {
                                            return Ok(ExprNode { kind: ExprVariant::Unary(unop, Box::new(new_expr)), node_id: self.ast.alloc_node_id(), span });
                                        }
                                    }
                                } else {
                                    // fallback: preserve Field node as before
                                    let ident = field_ident;
                                    let binop_expr = ExprNode { kind: ExprVariant::Field(inner, ident), node_id: self.ast.alloc_node_id(), span: binop_span };
                                    match unop {
                                        None => return Ok(binop_expr),
                                        Some(unop) => {
                                            return Ok(ExprNode { kind: ExprVariant::Unary(unop, Box::new(binop_expr)), node_id: self.ast.alloc_node_id(), span });
                                        }
                                    }
                                }
                            }
                            ExprVariant::Function(name, types, args) => {
                                // match for builtin function names and transform them into appropriate AST nodes
                                let signature = name.to_string();
                                let kind = match signature.as_str() {
                                    "last(default:)" => {
                                        assert_eq!(args.len(), 1);
                                        let lhs = ExprNode { kind: ExprVariant::SignalAccess(inner, AccessMode::Strict), node_id: self.ast.alloc_node_id(), span };
                                        ExprVariant::Default(Box::new(lhs), Box::new(args[0].clone()))
                                    }
                                    "prev()" => {
                                        let offset_node = ExprNode { kind: ExprVariant::Shift(inner, crate::ast::Shift::Discrete(-1)), node_id: self.ast.alloc_node_id(), span };
                                        offset_node.kind
                                    }
                                    "prev(default:)" => {
                                        assert_eq!(args.len(), 1);
                                        let offset_node = ExprNode { kind: ExprVariant::Shift(inner, crate::ast::Shift::Discrete(-1)), node_id: self.ast.alloc_node_id(), span };
                                        ExprVariant::Default(Box::new(offset_node), Box::new(args[0].clone()))
                                    }
                                    "history(index:)" | "history(index:,default:)" | "history(index:default:)" | "history(index,default:)"
                                    | "history(at:)" | "history(at:,default:)" | "history(at:default:)" | "history(at,default:)" => {
                                        let rhs_span = rhs.span;
                                        if args.len() == 1 {
                                            let offset_expr = &args[0];
                                            let offset = offset_expr.extract_discrete_shift().map_err(|reason| oorv_error_with_span(&format!("failed to parse offset: {}", reason), Some(rhs_span)))?;
                                            ExprVariant::Shift(inner, offset)
                                        } else if args.len() == 2 {
                                            // support combined form: history(index:-1,default:0)
                                            let offset_expr = &args[0];
                                            let offset = offset_expr.extract_discrete_shift().map_err(|reason| oorv_error_with_span(&format!("failed to parse offset: {}", reason), Some(rhs_span)))?;
                                            let offset_node = ExprNode { kind: ExprVariant::Shift(inner, offset), node_id: self.ast.alloc_node_id(), span };
                                            ExprVariant::Default(Box::new(offset_node), Box::new(args[1].clone()))
                                        } else {
                                            return Err(oorv_error_with_span("history expects 1 or 2 arguments", Some(rhs_span)));
                                        }
                                    }
                                    "defaults(value:)" => {
                                        assert_eq!(args.len(), 1);
                                        ExprVariant::Default(inner, Box::new(args[0].clone()))
                                    }
                                    _ => {
                                        // If this is a `format` call and the inner expression is
                                        // a string literal, validate and transform the placeholder
                                        // so that an empty `{}` (or `{   }`) becomes `{{}}`.
                                        let mut new_inner= inner.as_ref().clone();
                                        if name.name.name == "format" {
                                            match &inner.kind {
                                                ExprVariant::Literal(lit) => {
                                                    if let crate::ast::LiteralKind::Text(s) = &lit.kind {
                                                        // basic checks: braces must be balanced and no nested '{{' or '}}'
                                                        let open_count = s.matches('{').count();
                                                        let end_count = s.matches('}').count();
                                                        if open_count != end_count {
                                                            return Err(oorv_error_with_span("malformed braces in format string: mismatched number of '{' and '}'", Some(lit.span.clone())));
                                                        }
                                                        if s.contains("{{") || s.contains("}}") {
                                                            return Err(oorv_error_with_span("format string must not contain nested braces like '{{' or '}}'", Some(lit.span.clone())));
                                                        }

                                                        // locate all `{...}` pairs and validate each placeholder
                                                        let mut pairs: Vec<(usize, usize)> = Vec::new();
                                                        let mut idx: usize = 0;
                                                        while let Some(rel_open) = s[idx..].find('{') {
                                                            let open_pos = idx + rel_open;
                                                            if let Some(rel_end) = s[open_pos + 1..].find('}') {
                                                                let end_pos = open_pos + 1 + rel_end;
                                                                // ensure no stray braces inside the placeholder
                                                                if s[open_pos + 1..end_pos].contains('{') || s[open_pos + 1..end_pos].contains('}') {
                                                                    return Err(oorv_error_with_span("malformed braces in format string: mismatched braces", Some(lit.span.clone())));
                                                                }
                                                                // placeholder must be empty or whitespace-only
                                                                if s[open_pos + 1..end_pos].trim().len() != 0 {
                                                                    return Err(oorv_error_with_span("format placeholder must be empty or only contain whitespace: placeholder may not contain characters", Some(lit.span.clone())));
                                                                }
                                                                pairs.push((open_pos, end_pos));
                                                                idx = end_pos + 1;
                                                            } else {
                                                                return Err(oorv_error_with_span("malformed braces in format string: missing '}'", Some(lit.span.clone())));
                                                            }
                                                        }

                                                        if !pairs.is_empty() {
                                                            // ensure placeholder count matches number of args
                                                            if pairs.len() != args.len() {
                                                                return Err(oorv_error_with_span(&format!("format string contains {} placeholder(s) but {} argument(s) were provided: placeholder count and argument count mismatch", pairs.len(), args.len()), Some(lit.span.clone())));
                                                            }

                                                            // replace each `{...}` with `{{}}`, doing replacements in reverse
                                                            let mut new_s = s.clone();
                                                            for (open_pos, end_pos) in pairs.iter().rev() {
                                                                new_s.replace_range(*open_pos..(end_pos + 1), "{{}}");
                                                            }

                                                            let new_lit = crate::ast::TokenLiteral { kind: LiteralKind::Text(new_s.clone()), node_id: self.ast.alloc_node_id(), span: lit.span.clone() };
                                                            new_inner = ExprNode { kind: ExprVariant::Literal(new_lit), node_id: self.ast.alloc_node_id(), span: inner.span };
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        // fallback: just return Method as before
                                        ExprVariant::Method(Box::new(new_inner), name, types, args)
                                    }
                                };
                                let binop_expr = ExprNode { kind, node_id: self.ast.alloc_node_id(), span: binop_span };
                                match unop {
                                    None => return Ok(binop_expr),
                                    Some(unop) => {
                                        return Ok(ExprNode { kind: ExprVariant::Unary(unop, Box::new(binop_expr)), node_id: self.ast.alloc_node_id(), span });
                                    }
                                }
                            }
                            _ => {
                                return Err(oorv_error_with_span(&format!("expected method call or tuple access, found {:?}", rhs), Some(rhs.span)));
                            }
                        }
                    }
                    Rule::LBracket => {
                        let rhs_span = rhs.span;
                        let offset = rhs.extract_discrete_shift().map_err(|reason| oorv_error_with_span(&format!("failed to parse offset expression: {}", reason), Some(rhs_span)))?;
                        match lhs.kind {
                            ExprVariant::Unary(unop, inner) => {
                                let inner_span = Self::union_span(&inner.span, &rhs.span);
                                let new_inner = ExprNode { kind: ExprVariant::Shift(inner, offset), node_id: self.ast.alloc_node_id(), span: inner_span };
                                return Ok(ExprNode { kind: ExprVariant::Unary(unop, Box::new(new_inner)), node_id: self.ast.alloc_node_id(), span });
                            }
                            _ => {
                                return Ok(ExprNode { kind: ExprVariant::Shift(lhs.into(), offset), node_id: self.ast.alloc_node_id(), span });
                            }
                        }
                    }
                    _ => unreachable!(),
                };
                Ok(ExprNode { kind: ExprVariant::Binary(op, Box::new(lhs), Box::new(rhs)), node_id: self.ast.alloc_node_id(), span })
            }).parse(pairs)
    }

    fn construct_term_node(&self, pair: Pair<'_, Rule>) -> Result<ExprNode, OORVError> {
        // Convert pest span into our SourceSpan
        let full_span = pair.as_span();
        let to_source_span = |s: pest::Span| SourceSpan::Direct {
            start: s.start(),
            end: s.end(),
        };

        // Small helper for building ExprNode values consistently
        let make_node = |kind: ExprVariant, span: SourceSpan| ExprNode {
            kind,
            node_id: self.ast.alloc_node_id(),
            span,
        };

        match pair.as_rule() {
            Rule::ValueLit => Ok(make_node(
                ExprVariant::Literal(self.parse_value_literal(pair)),
                to_source_span(full_span),
            )),

            Rule::Ident => Ok(make_node(
                ExprVariant::Identifier(self.extract_ident(&pair)),
                to_source_span(full_span),
            )),

            Rule::ParenExpr => {
                let mut it = pair.into_inner();
                let open_tok = it.next().expect("parenthesized: missing open token");
                let expr_pair = it.next().expect("parenthesized: missing inner expression");
                let end_tok = it.next().expect("parenthesized: missing end token");

                if let Rule::MissingRParen = end_tok.as_rule() {
                    let msg = format!(
                        "Unclosed parenthesis (opened at {}..{}); parser reached {}..{} expecting a closing ')'.",
                        open_tok.as_span().start(),
                        open_tok.as_span().end(),
                        end_tok.as_span().start(),
                        end_tok.as_span().end(),
                    );
                    let span = SourceSpan::Direct {
                        start: open_tok.as_span().start(),
                        end: end_tok.as_span().end(),
                    };
                    return Err(oorv_error_with_span(&msg, Some(span)));
                }

                let inner_expr = self.construct_expr_node(expr_pair.into_inner(), None, None)?;
                Ok(make_node(
                    ExprVariant::Bracket(Box::new(inner_expr)),
                    to_source_span(full_span),
                ))
            }

            Rule::PrefixExpr => {
                let mut inner = pair.into_inner();
                let op_pair = inner.next().expect("unary: missing operator");
                let rhs_pair = inner.next().expect("unary: missing operand");
                let rhs = self.construct_term_node(rhs_pair)?;

                let op = match op_pair.as_rule() {
                    Rule::OpAdd => return Ok(rhs), // +x is a no-op
                    Rule::OpSub => UnaryOp::Neg,
                    Rule::OpNeg => UnaryOp::Not,
                    Rule::OpBitNot => UnaryOp::BitNot,
                    other => unreachable!("unknown unary operator: {:?}", other),
                };

                Ok(make_node(
                    ExprVariant::Unary(op, Box::new(rhs)),
                    to_source_span(full_span),
                ))
            }

            Rule::IfThenElse => {
                let mut parts = self.collect_expression_list(pair.into_inner())?;
                if parts.len() != 3 {
                    panic!("ternary expression must have three operands");
                }
                let a = Box::new(parts.remove(0));
                let b = Box::new(parts.remove(0));
                let c = Box::new(parts.remove(0));
                Ok(make_node(
                    ExprVariant::Ite(a, b, c),
                    to_source_span(full_span),
                ))
            }

            Rule::TupleExpr => {
                let items = self.collect_expression_list(pair.into_inner())?;
                if items.len() == 1 {
                    panic!("tuple must not contain exactly one element");
                }
                Ok(make_node(
                    ExprVariant::Tuple(items),
                    to_source_span(full_span),
                ))
            }

            Rule::Expr => self.construct_expr_node(pair.into_inner(), None, None),

            Rule::CallExpr => self.assemble_function_call(pair, to_source_span(full_span)),

            Rule::IntLit => {
                let s = to_source_span(full_span);
                let lit = TokenLiteral {
                    kind: LiteralKind::Number(pair.as_str().to_string(), None),
                    node_id: self.ast.alloc_node_id(),
                    span: s.clone(),
                };
                Ok(make_node(ExprVariant::Literal(lit), s))
            }

            Rule::MissingTerm => Ok(make_node(
                ExprVariant::MissingExpr,
                to_source_span(full_span),
            )),

            Rule::QuantifiedAlt => {
                let mut inner = pair.into_inner();
                let quant_pair = inner.next().expect("quantified: missing quantifier");
                let quant = match quant_pair.as_rule() {
                    Rule::Forall => Quantifier::Forall,
                    Rule::Exists => Quantifier::Exists,
                    _ => unreachable!("expected quantifier"),
                };

                let mut binds_a = Vec::new();
                let mut binds_b = Vec::new();
                let mut body_opt: Option<ExprNode> = None;

                for child in inner {
                    match child.as_rule() {
                        Rule::BindSetA => binds_a.extend(child.into_inner().filter_map(|idp| {
                            if idp.as_rule() == Rule::Ident {
                                Some(self.extract_ident(&idp))
                            } else {
                                None
                            }
                        })),
                        Rule::BindSetB => binds_b.extend(child.into_inner().filter_map(|idp| {
                            if idp.as_rule() == Rule::Ident {
                                Some(self.extract_ident(&idp))
                            } else {
                                None
                            }
                        })),
                        Rule::Expr => {
                            body_opt =
                                Some(self.construct_expr_node(child.into_inner(), None, None)?)
                        }
                        other => {
                            unreachable!("unexpected rule in quantified expression: {:?}", other)
                        }
                    }
                }

                let body = body_opt.expect("quantified expression missing body");
                Ok(make_node(
                    ExprVariant::Quantified(quant, binds_a, binds_b, Box::new(body)),
                    to_source_span(full_span),
                ))
            }

            other => unreachable!("unexpected term rule: {:?}", other),
        }
    }

    #[allow(clippy::vec_box)]
    fn collect_expression_list(&self, pairs: Pairs<'_, Rule>) -> Result<Vec<ExprNode>, OORVError> {
        let mut oks = Vec::new();
        let mut errs: Vec<String> = Vec::new();

        for p in pairs {
            match self.construct_expr_node(p.into_inner(), None, None) {
                Ok(n) => oks.push(n),
                Err(e) => errs.push(e.to_string()),
            }
        }

        if !errs.is_empty() {
            let msg = format!("one or more expression errors:\n{}", errs.join("\n"));
            return Err(oorv_error_with_span(&msg, None));
        }

        Ok(oks)
    }

    fn assemble_function_call(
        &self,
        pair: Pair<'_, Rule>,
        span: SourceSpan,
    ) -> Result<ExprNode, OORVError> {
        let mut parts = pair.into_inner();
        let callee = self.extract_ident(&parts.next().expect("function: expected name"));

        let mut head = parts.next().expect("function: expected args or generics");
        let generics = if head.as_rule() == Rule::TypeParams {
            let g = self.collect_type_list(head.into_inner());
            head = parts
                .next()
                .expect("function: expected args after generics");
            g
        } else {
            Vec::new()
        };

        debug_assert_eq!(head.as_rule(), Rule::CallArgs);
        let mut args = Vec::new();
        let mut names = Vec::new();

        for arg_pair in head.into_inner() {
            debug_assert_eq!(arg_pair.as_rule(), Rule::CallArg);
            let mut inner = arg_pair.into_inner();
            let first = inner.next().expect("function arg: missing first token");

            let (maybe_name, expr_pair) = if first.as_rule() == Rule::Ident {
                (
                    Some(self.extract_ident(&first)),
                    inner.next().expect("function arg: missing expr after name"),
                )
            } else {
                (None, first)
            };

            names.push(maybe_name);
            let node = self.construct_expr_node(expr_pair.into_inner(), None, None)?;
            args.push(node);
        }

        let label = FuncLabel {
            name: callee,
            arg_names: names,
        };
        Ok(ExprNode {
            kind: ExprVariant::Function(label, generics, args),
            node_id: self.ast.alloc_node_id(),
            span,
        })
    }

    fn collect_type_list(&self, pairs: Pairs<'_, Rule>) -> Vec<ValueType> {
        pairs.into_iter().map(|p| self.resolve_type(p)).collect()
    }

    fn parse_constrain_block(
        &self,
        pair: Pair<'_, Rule>,
        module_name: Option<Identifier>,
        class_name: Option<Identifier>,
        parameter_flag: bool,
    ) -> Result<Vec<Rc<Constrain>>, OORVError> {
        assert_eq!(pair.as_rule(), Rule::CheckBlock);
        let mut first_error: Option<OORVError> = None;
        let mut constrains: Vec<Rc<Constrain>> = Vec::new();
        for decl_pair in pair.into_inner() {
            if decl_pair.as_rule() == Rule::CheckItem {
                match self.parse_constrain_decl(
                    decl_pair,
                    module_name.clone(),
                    class_name.clone(),
                    parameter_flag,
                ) {
                    Ok(constrain) => {
                        constrains.extend(constrain);
                    }
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(constrains)
    }

    fn parse_constrain_decl(
        &self,
        pair: Pair<'_, Rule>,
        module_name: Option<Identifier>,
        class_name: Option<Identifier>,
        parameter_flag: bool,
    ) -> Result<Vec<Rc<Constrain>>, OORVError> {
        assert_eq!(pair.as_rule(), Rule::CheckItem);
        let mut constrains: Vec<Rc<Constrain>> = Vec::new();

        let override_flag = false;

        let mut eval = Vec::new();
        let mut start: Option<StartDecl> = None;
        let mut end: Option<EndDecl> = None;

        let span_pair = pair.as_span();
        let span_inv = SourceSpan::Direct {
            start: span_pair.start(),
            end: span_pair.end(),
        };
        let mut inner = pair.into_inner();
        let _name_pair = inner.next().expect("Expected Identifier in FunDecl");
        let mut kind = ConstrainKind::Alarm;
        let mut level: Option<Identifier> = None;

        // Saved condition from the preceding if-branch.
        let mut if_condition_pair: Option<Pair<'_, Rule>> = None;
        let mut start_children = inner.peekable();
        let next_pair = start_children.peek();

        let mut first_error: Option<OORVError> = None;
        let annotated_type: Option<ValueType> = None;
        let mut params = Vec::new();

        // Activation condition.
        let annotated_pacing = if let Some(pair) = next_pair {
            if let Rule::Timing = pair.as_rule() {
                let expr = self.parse_activation(start_children.next().unwrap());
                expr.map_or_else(
                    |e| {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                        None
                    },
                    Some,
                )
            } else {
                None
            }
        } else {
            None
        }
        .unwrap_or_else(|| PacingNode::NotAnnotated(SourceSpan::Unknown));

        for child in start_children {
            match child.as_rule() {
                Rule::LetStmt => {
                    let annotated_pacing_letdecl = PacingNode::NotAnnotated(SourceSpan::Unknown);
                    eval.clear();
                    start = None;
                    end = None;
                    params.clear();
                    let inners = child.into_inner();
                    inners.for_each(|pair| match pair.as_rule() {
                        Rule::LetName => {
                            let mut inners_child = pair.into_inner().peekable();
                            let name_pair =
                                inners_child.next().expect("Expected Identifier in FunDecl");
                            let orig_name = self.extract_ident(&name_pair);
                            let name_let = if let (Some(m), Some(c)) = (&module_name, &class_name) {
                                Identifier {
                                    name: format!("{}::{}::{}", m.name, c.name, orig_name.name),
                                    span: orig_name.span.clone(),
                                }
                            } else if let Some(m) = &module_name {
                                Identifier {
                                    name: format!("{}::{}", m.name, orig_name.name),
                                    span: orig_name.span.clone(),
                                }
                            } else if let Some(c) = &class_name {
                                Identifier {
                                    name: format!("{}::{}", c.name, orig_name.name),
                                    span: orig_name.span.clone(),
                                }
                            } else {
                                orig_name
                            };
                            // Skip param parsing; build default id param only when parameter_flag is set.
                            params = if parameter_flag {
                                vec![ParamDecl {
                                    name: Identifier {
                                        name: "id".to_string(),
                                        span: SourceSpan::Unknown,
                                    },
                                    annotation: None,
                                    position: 0,
                                    node_id: self.ast.alloc_node_id(),
                                    span: SourceSpan::Unknown,
                                }]
                            } else {
                                Vec::new()
                            };
                            kind = ConstrainKind::Output(name_let.clone());
                            let uid_str = match (&module_name, &class_name) {
                                (Some(m), Some(c)) => format!("{}::{}::uid", m.name, c.name),
                                (None, Some(c)) => format!("{}::uid", c.name),
                                (Some(m), None) => format!("{}::uid", m.name),
                                (None, None) => "uid".to_string(),
                            };
                            let expr_ident = Identifier {
                                name: uid_str,
                                span: SourceSpan::Unknown,
                            };
                            let expr = ExprNode {
                                kind: ExprVariant::Identifier(expr_ident.clone()),
                                node_id: self.ast.alloc_node_id(),
                                span: SourceSpan::Unknown,
                            };
                            // Build default start param only when parameter_flag is set.
                            start = if parameter_flag {
                                Some(StartDecl {
                                    pacing: annotated_pacing_letdecl.clone(),
                                    condition: None,
                                    expression: Some(expr),
                                    node_id: self.ast.alloc_node_id(),
                                    span: SourceSpan::Unknown,
                                })
                            } else {
                                None
                            };
                        }
                        Rule::LetValue => {
                            // Build condition: id == uid.
                            let left_ident = Identifier {
                                name: "id".to_string(),
                                span: SourceSpan::Unknown,
                            };
                            let left_expr = ExprNode {
                                kind: ExprVariant::Identifier(left_ident.clone()),
                                node_id: self.ast.alloc_node_id(),
                                span: SourceSpan::Unknown,
                            };
                            let right_uid_str = match (&module_name, &class_name) {
                                (Some(m), Some(c)) => format!("{}::{}::uid", m.name, c.name),
                                (None, Some(c)) => format!("{}::uid", c.name),
                                (Some(m), None) => format!("{}::uid", m.name),
                                (None, None) => "uid".to_string(),
                            };
                            let right_ident = Identifier {
                                name: right_uid_str,
                                span: SourceSpan::Unknown,
                            };
                            let right_expr = ExprNode {
                                kind: ExprVariant::Identifier(right_ident.clone()),
                                node_id: self.ast.alloc_node_id(),
                                span: SourceSpan::Unknown,
                            };
                            let cond_expr = ExprNode {
                                kind: ExprVariant::Binary(
                                    BinaryOp::Eq,
                                    Box::new(left_expr),
                                    Box::new(right_expr),
                                ),
                                node_id: self.ast.alloc_node_id(),
                                span: SourceSpan::Unknown,
                            };
                            let condition = if parameter_flag {
                                Some(cond_expr)
                            } else {
                                None
                            };
                            // Pass the constructed condition to parse_evaldecl.
                            let eval_spec = self.parse_evaldecl(
                                pair,
                                annotated_pacing_letdecl.clone(),
                                condition,
                                module_name.clone(),
                                class_name.clone(),
                            );
                            match eval_spec {
                                Ok(eval_spec) => {
                                    debug_assert!(
                                        eval.is_empty(),
                                        "must be empty due to grammar restrictions"
                                    );
                                    eval.push(eval_spec)
                                }
                                Err(e) => {
                                    if first_error.is_none() {
                                        first_error = Some(e);
                                    }
                                }
                            };
                            let constrain = Constrain {
                                kind: kind.clone(),
                                annotation: annotated_type.clone(),
                                module_name: module_name.clone(),
                                class_name: class_name.clone(),
                                override_flag: override_flag.clone(),
                                eval: eval.clone(),
                                start: start.clone(),
                                end: end.clone(),
                                params: params.clone().into_iter().map(Rc::new).collect(),
                                level: level.clone(),
                                node_id: self.ast.alloc_node_id(),
                                span: span_inv.clone(),
                            };
                            constrains.push(Rc::new(constrain));
                        }
                        _ => {}
                    });
                }
                Rule::BranchGroup => {
                    let start_anntype = PacingNode::NotAnnotated(SourceSpan::Unknown);
                    let inners = child.into_inner();
                    inners.for_each(|pair| match pair.as_rule() {
                        Rule::CondBranch => {
                            eval.clear();
                            start = None;
                            end = None;
                            params.clear();
                            // Skip param parsing; build default id param only when parameter_flag is set.
                            params = if parameter_flag {
                                vec![ParamDecl {
                                    name: Identifier {
                                        name: "id".to_string(),
                                        span: SourceSpan::Unknown,
                                    },
                                    annotation: None,
                                    position: 0,
                                    node_id: self.ast.alloc_node_id(),
                                    span: SourceSpan::Unknown,
                                }]
                            } else {
                                Vec::new()
                            };

                            let inners_child = pair.into_inner();
                            inners_child.for_each(|pair_child| match pair_child.as_rule() {
                                // Parse start declaration.
                                Rule::BranchHead => {
                                    if let Some(_old_start) = &start {
                                        let s = pair_child.as_span();
                                        let span_direct = SourceSpan::Direct {
                                            start: s.start(),
                                            end: s.end(),
                                        };
                                        if first_error.is_none() {
                                            first_error = Some(oorv_error_with_span(
                                                "Multiple Start clauses found",
                                                Some(span_direct),
                                            ));
                                        }
                                    }

                                    let result = self.parse_startdecl(
                                        pair_child,
                                        start_anntype.clone(),
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match result {
                                        Ok((start_spec, pair)) => {
                                            (start, if_condition_pair) =
                                                (Some(start_spec), Some(pair));
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    }
                                }
                                // Parse kind and eval declarations.
                                Rule::InfoOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "info".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    // Forward start condition to eval.
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                // Build cond_expr: id == module::class::uid.
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };

                                                // AND-combine eval_spec.condition with cond_expr, or use cond_expr directly.
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }

                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        annotation: annotated_type.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                Rule::AlertOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "alert".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }
                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        annotation: annotated_type.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                Rule::ViolationOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "violation".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                // Build cond_expr: id == module::class::uid.
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };

                                                // AND-combine eval_spec.condition with cond_expr, or use cond_expr directly.
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }

                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        annotation: annotated_type.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                _ => {}
                            });
                        }
                        Rule::ElseBranch => {
                            eval.clear();
                            start = None;
                            end = None;
                            params.clear();
                            // Skip param parsing; build default id param only when parameter_flag is set.
                            params = if parameter_flag {
                                vec![ParamDecl {
                                    name: Identifier {
                                        name: "id".to_string(),
                                        span: SourceSpan::Unknown,
                                    },
                                    annotation: None,
                                    position: 0,
                                    node_id: self.ast.alloc_node_id(),
                                    span: SourceSpan::Unknown,
                                }]
                            } else {
                                Vec::new()
                            };
                            // Build a negated start from the preceding if-branch condition.
                            if let Some(cond_pair) = &if_condition_pair {
                                // build original condition expression
                                match self.construct_expr_node(
                                    cond_pair.clone().into_inner(),
                                    module_name.clone(),
                                    class_name.clone(),
                                ) {
                                    Ok(orig_expr) => {
                                        // parenthesize then negate
                                        let paren = ExprNode {
                                            kind: ExprVariant::Bracket(Box::new(orig_expr.clone())),
                                            node_id: self.ast.alloc_node_id(),
                                            span: SourceSpan::Unknown,
                                        };
                                        let neg = ExprNode {
                                            kind: ExprVariant::Unary(UnaryOp::Not, Box::new(paren)),
                                            node_id: self.ast.alloc_node_id(),
                                            span: SourceSpan::Unknown,
                                        };
                                        // construct uid expression (qualified to module::class::uid when possible)
                                        let uid_str = match (&module_name, &class_name) {
                                            (Some(m), Some(c)) => {
                                                format!("{}::{}::uid", m.name, c.name)
                                            }
                                            (None, Some(c)) => format!("{}::uid", c.name),
                                            (Some(m), None) => format!("{}::uid", m.name),
                                            (None, None) => "uid".to_string(),
                                        };
                                        let expr_ident = Identifier {
                                            name: uid_str,
                                            span: SourceSpan::Unknown,
                                        };
                                        let expr = ExprNode {
                                            kind: ExprVariant::Identifier(expr_ident.clone()),
                                            node_id: self.ast.alloc_node_id(),
                                            span: SourceSpan::Unknown,
                                        };
                                        start = Some(StartDecl {
                                            expression: Some(expr),
                                            pacing: start_anntype.clone(),
                                            condition: Some(neg),
                                            node_id: self.ast.alloc_node_id(),
                                            span: SourceSpan::Unknown,
                                        });
                                    }
                                    Err(e) => {
                                        if first_error.is_none() {
                                            first_error = Some(e);
                                        }
                                    }
                                }
                            }

                            let inners_child = pair.into_inner();
                            inners_child.for_each(|pair_child| match pair_child.as_rule() {
                                // Else branch: start condition is the negation of the if-condition.
                                Rule::InfoOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "info".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                // Build cond_expr: id == module::class::uid.
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };

                                                // AND-combine eval_spec.condition with cond_expr, or use cond_expr directly.
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }

                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    // Clear start condition while preserving other fields.
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        annotation: annotated_type.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                Rule::AlertOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "alert".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                // Build cond_expr: id == module::class::uid.
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };

                                                // AND-combine eval_spec.condition with cond_expr, or use cond_expr directly.
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }

                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        annotation: annotated_type.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                Rule::ViolationOutput => {
                                    kind = ConstrainKind::Alarm;
                                    level = Some(Identifier {
                                        name: "violation".to_string(),
                                        span: SourceSpan::Direct {
                                            start: pair_child.as_span().start(),
                                            end: pair_child.as_span().end(),
                                        },
                                    });
                                    let eval_condition = match start {
                                        Some(ref s) => s.condition.clone(),
                                        None => None,
                                    };
                                    let eval_spec = self.parse_evaldecl(
                                        pair_child,
                                        annotated_pacing.clone(),
                                        eval_condition,
                                        module_name.clone(),
                                        class_name.clone(),
                                    );
                                    match eval_spec {
                                        Ok(mut eval_spec) => {
                                            debug_assert!(
                                                eval.is_empty(),
                                                "must be empty due to grammar restrictions"
                                            );
                                            if parameter_flag {
                                                // Build cond_expr: id == module::class::uid.
                                                let left_ident = Identifier {
                                                    name: "id".to_string(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let left_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        left_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_uid_str =
                                                    match (&module_name, &class_name) {
                                                        (Some(m), Some(c)) => {
                                                            format!("{}::{}::uid", m.name, c.name)
                                                        }
                                                        (None, Some(c)) => {
                                                            format!("{}::uid", c.name)
                                                        }
                                                        (Some(m), None) => {
                                                            format!("{}::uid", m.name)
                                                        }
                                                        (None, None) => "uid".to_string(),
                                                    };
                                                let right_ident = Identifier {
                                                    name: right_uid_str,
                                                    span: SourceSpan::Unknown,
                                                };
                                                let right_expr = ExprNode {
                                                    kind: ExprVariant::Identifier(
                                                        right_ident.clone(),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };
                                                let cond_expr = ExprNode {
                                                    kind: ExprVariant::Binary(
                                                        BinaryOp::Eq,
                                                        Box::new(left_expr),
                                                        Box::new(right_expr),
                                                    ),
                                                    node_id: self.ast.alloc_node_id(),
                                                    span: SourceSpan::Unknown,
                                                };

                                                // AND-combine eval_spec.condition with cond_expr, or use cond_expr directly.
                                                eval_spec.condition = match eval_spec.condition {
                                                    Some(existing) => Some(ExprNode {
                                                        kind: ExprVariant::Binary(
                                                            BinaryOp::And,
                                                            Box::new(existing),
                                                            Box::new(cond_expr),
                                                        ),
                                                        node_id: self.ast.alloc_node_id(),
                                                        span: SourceSpan::Unknown,
                                                    }),
                                                    None => Some(cond_expr),
                                                };
                                            }

                                            eval.push(eval_spec)
                                        }
                                        Err(e) => {
                                            if first_error.is_none() {
                                                first_error = Some(e);
                                            }
                                        }
                                    };
                                    let mut start_for_constrain = start.clone().map(|mut s| {
                                        s.condition = None;
                                        s
                                    });
                                    // Clear start when parameter_flag is false.
                                    if !parameter_flag {
                                        start_for_constrain = None;
                                    }
                                    let constrain = Constrain {
                                        kind: kind.clone(),
                                        module_name: module_name.clone(),
                                        class_name: class_name.clone(),
                                        override_flag: override_flag.clone(),
                                        eval: eval.clone(),
                                        start: start_for_constrain.clone(),
                                        end: end.clone(),
                                        annotation: annotated_type.clone(),
                                        params: params.clone().into_iter().map(Rc::new).collect(),
                                        level: level.clone(),
                                        node_id: self.ast.alloc_node_id(),
                                        span: span_inv.clone(),
                                    };
                                    constrains.push(Rc::new(constrain));
                                }
                                _ => {}
                            });
                        }
                        _ => {}
                    });
                }
                _ => {
                    // ignore other tokens
                }
            }
        }

        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(constrains)
    }

    fn parse_class(
        &mut self,
        pair: Pair<'_, Rule>,
        module_name: Option<Identifier>,
        use_set: &BTreeSet<String>,
        parameter_flag: bool,
    ) -> Result<ClassDecl, OORVError> {
        assert_eq!(pair.as_rule(), Rule::TypeDef);
        let mut first_error: Option<OORVError> = None;
        let span = {
            let s = pair.as_span();
            SourceSpan::Direct {
                start: s.start(),
                end: s.end(),
            }
        };
        let mut pairs = pair.into_inner().peekable();
        let name = self.extract_ident(&pairs.next().expect("parse error"));

        let pair = pairs.peek().expect("parse error");
        let base_class = if pair.as_rule() == Rule::Ident {
            Some(self.extract_ident(&pairs.next().expect("parse error")))
        } else {
            None
        };

        let classbody_pair = pairs.next().expect("parse error");
        assert_eq!(classbody_pair.as_rule(), Rule::TypeBody);
        let mut signals: Vec<Rc<Signal>> = Vec::new();
        let constrains: Vec<Rc<Constrain>> = Vec::new();

        for pair in classbody_pair.into_inner() {
            match pair.as_rule() {
                Rule::StreamBlock => {
                    match self.collect_signals_block(pair, module_name.clone(), Some(name.clone()))
                    {
                        Ok(signals_vec) => signals.extend(signals_vec),
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                Rule::CheckBlock => {
                    match self.parse_constrain_block(
                        pair,
                        module_name.clone(),
                        Some(name.clone()),
                        parameter_flag,
                    ) {
                        Ok(constrain) => self.ast.constrains.extend(constrain),
                        Err(e) => {
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }

        Ok(ClassDecl {
            name,
            module_name: module_name,
            base_class,
            signals,
            constrains,
            uses: use_set.clone(),
            node_id: self.ast.alloc_node_id(),
            span,
        })
    }

    fn parse_activation(&self, pair: Pair<'_, Rule>) -> Result<PacingNode, OORVError> {
        debug_assert_eq!(pair.as_rule(), Rule::Timing);
        let mut it = pair.into_inner();
        let item = it.next().expect("ActivationCondition must contain a child");

        match item.as_rule() {
            Rule::GlobalTiming => {
                let child = item
                    .into_inner()
                    .next()
                    .expect("GlobalActivationCondition missing child");
                let expr = self.parse_activation_expr(child)?;
                Ok(PacingNode::Global(expr))
            }
            Rule::LocalTiming => {
                let child = item
                    .into_inner()
                    .next()
                    .expect("LocalActivationCondition missing child");
                let expr = self.parse_activation_expr(child)?;
                Ok(PacingNode::Local(expr))
            }
            Rule::TimingExpr => {
                let expr = self.parse_activation_expr(item)?;
                Ok(PacingNode::Unspecified(expr))
            }
            other => unreachable!("unexpected ActivationCondition variant: {:?}", other),
        }
    }

    fn parse_activation_expr(&self, pair: Pair<'_, Rule>) -> Result<ExprNode, OORVError> {
        debug_assert_eq!(pair.as_rule(), Rule::TimingExpr);
        let child = pair.into_inner().next().expect("AcExpr must have a child");
        let span = {
            let s = child.as_span();
            SourceSpan::Direct {
                start: s.start(),
                end: s.end(),
            }
        };

        match child.as_rule() {
            Rule::NumLit => Ok(ExprNode {
                kind: ExprVariant::Literal(self.parse_numeric_literal(child)),
                node_id: self.ast.alloc_node_id(),
                span,
            }),
            Rule::GuardExpr => self.parse_pos_bool_expr(child.into_inner()),
            other => unreachable!("unexpected AcExpr variant: {:?}", other),
        }
    }

    fn parse_pos_bool_expr(&self, pairs: Pairs<Rule>) -> Result<ExprNode, OORVError> {
        pratt_parser()
            .map_primary(|primary| match primary.as_rule() {
                Rule::GuardExpr => self.parse_pos_bool_expr(primary.into_inner()),
                Rule::Ident => {
                    let s = primary.as_span();
                    let span = SourceSpan::Direct {
                        start: s.start(),
                        end: s.end(),
                    };
                    Ok(ExprNode {
                        kind: ExprVariant::Identifier(self.extract_ident(&primary)),
                        node_id: self.ast.alloc_node_id(),
                        span,
                    })
                }
                Rule::BoolTrue | Rule::BoolAlways => {
                    let s = primary.as_span();
                    let span = SourceSpan::Direct {
                        start: s.start(),
                        end: s.end(),
                    };
                    let lit = TokenLiteral {
                        kind: LiteralKind::Boolean(true),
                        node_id: self.ast.alloc_node_id(),
                        span: span.clone(),
                    };
                    Ok(ExprNode {
                        kind: ExprVariant::Literal(lit),
                        node_id: self.ast.alloc_node_id(),
                        span,
                    })
                }
                other => unreachable!("unexpected primary in PositiveBooleanExpr: {:?}", other),
            })
            .map_infix(|lhs, op, rhs| {
                let lhs = lhs?;
                let rhs = rhs?;
                let span = Self::union_span(&lhs.span, &rhs.span);
                let op = match op.as_rule() {
                    Rule::OpAnd | Rule::OpBitAnd => BinaryOp::And,
                    Rule::OpOr | Rule::OpBitOr => BinaryOp::Or,
                    other => unreachable!(
                        "unexpected infix operator in PositiveBooleanExpr: {:?}",
                        other
                    ),
                };
                Ok(ExprNode {
                    kind: ExprVariant::Binary(op, Box::new(lhs), Box::new(rhs)),
                    node_id: self.ast.alloc_node_id(),
                    span,
                })
            })
            .parse(pairs)
    }

    fn parse_startdecl<'b>(
        &self,
        start_pair: Pair<'b, Rule>,
        annotated_pacing: PacingNode,
        module_name: Option<Identifier>,
        class_name: Option<Identifier>,
    ) -> Result<(StartDecl, Pair<'b, Rule>), OORVError> {
        let span = start_pair.as_span();
        let span_start: SourceSpan = SourceSpan::Direct {
            start: span.start(),
            end: span.end(),
        };

        // default expression references the class/module uid (if any)
        let uid = match (&module_name, &class_name) {
            (Some(m), Some(c)) => format!("{}::{}::uid", m.name, c.name),
            (None, Some(c)) => format!("{}::uid", c.name),
            (Some(m), None) => format!("{}::uid", m.name),
            (None, None) => "uid".to_string(),
        };
        let default_ident = Identifier {
            name: uid,
            span: SourceSpan::Unknown,
        };
        let default_expr = ExprNode {
            kind: ExprVariant::Identifier(default_ident.clone()),
            node_id: self.ast.alloc_node_id(),
            span: SourceSpan::Unknown,
        };

        // try to extract a possible condition child (grammar dictates a single child)
        let condition_child: Pair<'b, Rule> = match start_pair.as_rule() {
            Rule::BranchHead => start_pair.clone().into_inner().next().expect("parse error"),
            _ => unreachable!("parse error"),
        };

        // build the condition expression and propagate any parser error
        let cond_expr = self.construct_expr_node(
            condition_child.clone().into_inner(),
            module_name.clone(),
            class_name.clone(),
        )?;
        let condition = Some(cond_expr);

        // `default_expr` is an Identifier by construction; keep a runtime check
        if let ExprVariant::Identifier(_) = &default_expr.kind {
            if condition.is_none() && matches!(annotated_pacing, PacingNode::NotAnnotated(_)) {
                // keep message consistent with previous API
                return Err(oorv_error_with_span(
                    "Start clause requires a condition, expression or pacing",
                    Some(span_start),
                ));
            }
        }

        Ok((
            StartDecl {
                pacing: annotated_pacing,
                condition,
                expression: Some(default_expr),
                node_id: self.ast.alloc_node_id(),
                span: span_start,
            },
            condition_child,
        ))
    }

    fn parse_evaldecl(
        &self,
        end_pair: Pair<'_, Rule>,
        annotated_pacing: PacingNode,
        condition: Option<ExprNode>,
        module_name: Option<Identifier>,
        class_name: Option<Identifier>,
    ) -> Result<EvalDecl, OORVError> {
        let s = end_pair.as_span();
        let span_end: SourceSpan = SourceSpan::Direct {
            start: s.start(),
            end: s.end(),
        };

        // extract the expression child according to grammar
        let expr_child = match end_pair.as_rule() {
            Rule::InfoOutput | Rule::AlertOutput | Rule::ViolationOutput | Rule::LetValue => {
                end_pair.into_inner().next().expect("parse error")
            }
            _ => unreachable!("parse error"),
        };

        let expr_node = self.construct_expr_node(
            expr_child.into_inner(),
            module_name.clone(),
            class_name.clone(),
        )?;

        if matches!(annotated_pacing, PacingNode::NotAnnotated(_))
            && condition.is_none()
            && expr_node.kind == ExprVariant::MissingExpr
        {
            return Err(oorv_error_with_span(
                "End clause requires either an expression or a condition",
                Some(span_end),
            ));
        }

        Ok(EvalDecl {
            pacing: annotated_pacing,
            condition,
            expression: Some(expr_node),
            node_id: self.ast.alloc_node_id(),
            span: span_end,
        })
    }

    pub(crate) fn parse_rational(repr: &str) -> Result<Rational, String> {
        debug_assert!(repr.parse::<f64>().is_ok());

        // Split off an exponent part (e or E) if present
        let (mantissa_part, expo_part) = match repr.find(|c| c == 'e' || c == 'E') {
            Some(idx) => (&repr[..idx], Some(&repr[idx + 1..])),
            None => (repr, None),
        };

        // Split mantissa into integer and fractional components
        let mut int_and_frac = mantissa_part.splitn(2, '.');
        let int_part = int_and_frac.next().unwrap_or("");
        let frac_part = int_and_frac.next().unwrap_or("");

        // Build a BigInt representing all digits (sign is kept in int_part)
        let combined = format!("{}{}", int_part, frac_part);
        let big_int = BigInt::from_str(combined.as_str())
            .map_err(|e| format!("could not parse digits of rational '{repr}': {e}"))?;

        let mut rat = BigRational::from(big_int);

        // If we have fractional digits, divide by 10^{frac_len}
        if !frac_part.is_empty() {
            let ten: BigInt = BigInt::from(10u8);
            let denom = ten.pow(frac_part.len());
            rat /= denom;
        }

        // Apply exponent if present
        if let Some(e_str) = expo_part {
            if !e_str.is_empty() {
                // parse exponent as signed integer, but restrict to i16 range
                let exp_i64 = BigInt::from_str(e_str)
                    .map_err(|e| format!("invalid exponent in rational '{repr}': {e}"))?;
                let exp = exp_i64.to_i16().ok_or_else(|| {
                    format!("exponent {exp_i64} in '{repr}' out of supported range")
                })?;

                let ten: BigInt = BigInt::from(10u8);
                let abs_exp = (exp.abs() as u32) as usize;
                let factor = ten.pow(abs_exp);
                if exp < 0 {
                    rat /= factor;
                } else {
                    rat *= factor;
                }
            }
        }

        // Convert to Rational64 (i64 numerator/denominator)
        match (rat.numer().to_i64(), rat.denom().to_i64()) {
            (Some(n), Some(d)) => Ok(Rational::from((n, d))),
            _ => Err(format!(
                "parsed rational '{repr}' is out of Rational64 bounds: {rat}"
            )),
        }
    }
}

fn preprocess_include(
    pairs: Pairs<Rule>,
    spec: &str,
    base_dir: Option<&std::path::Path>,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) -> Result<String, OORVError> {
    use std::fs;
    use std::path::PathBuf;

    // collect replacements as (start, end, replacement_text)
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    // Walk the parse tree and for each IncludeStat produce a replacement which is the
    // processed contents of the included file (recursively processed).
    fn walk(
        pair: Pair<Rule>,
        spec: &str,
        replacements: &mut Vec<(usize, usize, String)>,
        base_dir: Option<&std::path::Path>,
        seen: &mut std::collections::HashSet<std::path::PathBuf>,
    ) -> Result<(), OORVError> {
        match pair.as_rule() {
            Rule::IncludeStat => {
                // IncludeStat -> Literal -> String
                // capture span before we move `pair` into inner
                let span = pair.as_span();
                let mut inner = pair.into_inner();
                if let Some(lit) = inner.next() {
                    if let Some(str_pair) = lit.into_inner().next() {
                        let path_str = str_pair.as_str().to_string();

                        let p = PathBuf::from(&path_str);
                        let candidate = if p.is_absolute() {
                            p
                        } else if let Some(b) = base_dir {
                            b.join(p)
                        } else {
                            std::env::current_dir()
                                .unwrap_or_else(|_| PathBuf::from("."))
                                .join(p)
                        };

                        // canonicalize when possible to detect cycles; fall back to candidate
                        let canonical = match fs::canonicalize(&candidate) {
                            Ok(c) => c,
                            Err(_) => candidate.clone(),
                        };

                        if seen.contains(&canonical) {
                            let msg = format!(
                                "Include cycle or repeated include detected: {}",
                                canonical.display()
                            );
                            let span_direct = SourceSpan::Direct {
                                start: span.start(),
                                end: span.end(),
                            };
                            return Err(oorv_error_with_span(&msg, Some(span_direct)));
                        }

                        match fs::read_to_string(&candidate) {
                            Ok(contents) => {
                                // mark as seen before recursing
                                seen.insert(canonical.clone());
                                // parse included contents to find nested includes
                                let inner_pairs = match OORVRule::parse(Rule::Spec, &contents) {
                                    Ok(ps) => ps,
                                    Err(e) => {
                                        let d = OORVSpecParser::format_pest_error(
                                            e,
                                            &candidate.display().to_string(),
                                            &contents,
                                        );
                                        return Err(OORVError::from(d));
                                    }
                                };
                                let child_base = candidate.parent();
                                let processed_child =
                                    preprocess_include(inner_pairs, &contents, child_base, seen)?;
                                // unmark so that other include sites can include same file if desired
                                seen.remove(&canonical);

                                replacements.push((span.start(), span.end(), processed_child));
                            }
                            Err(e) => {
                                let msg = format!(
                                    "Failed to read include file {}: {}",
                                    candidate.display(),
                                    e
                                );
                                let span_direct = SourceSpan::Direct {
                                    start: span.start(),
                                    end: span.end(),
                                };
                                return Err(oorv_error_with_span(&msg, Some(span_direct)));
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => {
                for inner in pair.into_inner() {
                    walk(inner, spec, replacements, base_dir, seen)?;
                }
                Ok(())
            }
        }
    }

    for top in pairs {
        walk(top, spec, &mut replacements, base_dir, seen)?;
    }

    if replacements.is_empty() {
        return Ok(spec.to_string());
    }

    // apply replacements in reverse order so indices remain valid
    replacements.sort_by(|a, b| b.0.cmp(&a.0));
    let mut new_spec = String::with_capacity(spec.len());
    let mut last = spec.len();
    for (start, end, repl) in replacements.iter() {
        if *end < last {
            new_spec.insert_str(0, &spec[*end..last]);
        }
        new_spec.insert_str(0, repl);
        last = *start;
    }
    if last > 0 {
        new_spec.insert_str(0, &spec[0..last]);
    }

    Ok(new_spec)
}

fn preprocess_quantifiers(pairs: Pairs<Rule>, spec: &str) -> Result<String, OORVError> {
    // collect replacements as (start, end, replacement)
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    // helper: is identifier char (approx): letter, digit, '_' or ':' or '\''
    fn is_ident_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == ':' || c == '\''
    }

    // replace occurrences of any of the `var_names` as identifiers in `body` with their
    // corresponding values from `mapping`. Also return the sequence of variable names in
    // the order they were replaced (left-to-right in `body`).
    fn replace_idents_with_order(
        body: &str,
        var_names: &Vec<String>,
        mapping: &std::collections::HashMap<String, String>,
    ) -> (String, Vec<String>) {
        let mut out = String::with_capacity(body.len());
        let mut i = 0usize;
        let mut order: Vec<String> = Vec::new();

        while i < body.len() {
            let mut best_pos: Option<usize> = None;
            let mut best_var: Option<&String> = None;

            for v in var_names.iter() {
                if let Some(found) = body[i..].find(v) {
                    let pos = i + found;
                    // check boundaries
                    let prev_ok = if pos == 0 {
                        true
                    } else {
                        let ch = body[..pos].chars().rev().next().unwrap();
                        !is_ident_char(ch)
                    };
                    let after_idx = pos + v.len();
                    let next_ok = if after_idx >= body.len() {
                        true
                    } else {
                        let ch2 = body[after_idx..].chars().next().unwrap();
                        !is_ident_char(ch2)
                    };
                    if prev_ok && next_ok {
                        if best_pos.is_none() || pos < best_pos.unwrap() {
                            best_pos = Some(pos);
                            best_var = Some(v);
                        }
                    }
                }
            }

            if let Some(pos) = best_pos {
                let var = best_var.unwrap();
                out.push_str(&body[i..pos]);
                let replacement = mapping.get(var).map(|s| s.as_str()).unwrap_or("");
                out.push_str(replacement);
                order.push(var.clone());
                i = pos + var.len();
            } else {
                break;
            }
        }

        out.push_str(&body[i..]);
        (out, order)
    }

    // recursive walk
    fn walk(pair: Pair<Rule>, spec: &str, replacements: &mut Vec<(usize, usize, String)>) {
        match pair.as_rule() {
            Rule::Quantified => {
                // Extract inner: quantifier, varbindings..., Expr
                let mut inner = pair.clone().into_inner();
                let quant_pair = inner.next();
                if quant_pair.is_none() {
                    return;
                }
                let quant = quant_pair.unwrap();
                // collect varbindings
                let mut bindings = Vec::new();
                while let Some(p) = inner.clone().next() {
                    if p.as_rule() == Rule::VarBind {
                        let vb = inner.next().unwrap();
                        bindings.push(vb);
                        continue;
                    }
                    break;
                }

                // next should be the expression (body)
                let expr_pair_opt = inner.find(|p| matches!(p.as_rule(), Rule::Expr));
                if expr_pair_opt.is_none() {
                    return;
                }
                let expr_pair = expr_pair_opt.unwrap();

                // Separate named collection domains from explicit-list domains.
                let mut named_var_names: Vec<String> = Vec::new();
                let mut named_domain_src: Vec<String> = Vec::new();
                let mut list_var_names: Vec<String> = Vec::new();
                let mut list_domains: Vec<Vec<String>> = Vec::new();
                let mut all_lists = true;
                for vb in bindings.iter() {
                    let mut vb_inner = vb.clone().into_inner();
                    let name_pair = vb_inner.next();
                    let domain_pair = vb_inner.next();
                    if name_pair.is_none() || domain_pair.is_none() {
                        all_lists = false;
                        break;
                    }
                    let name = name_pair.unwrap().as_str().trim().to_string();
                    let domain_p = domain_pair.unwrap();
                    // keep original textual domain for later reporting
                    let domain_text = domain_p.as_str().trim().to_string();
                    match domain_p.as_rule() {
                        Rule::DomainList => {
                            let ids: Vec<String> = domain_p
                                .into_inner()
                                .map(|idp| idp.as_str().trim().to_string())
                                .collect();
                            list_var_names.push(name);
                            list_domains.push(ids);
                        }
                        Rule::DomainPath | Rule::Ident => {
                            named_var_names.push(name);
                            named_domain_src.push(domain_text);
                        }
                        _ => {
                            all_lists = false;
                            break;
                        }
                    }
                }

                if !all_lists {
                    return;
                }

                let quant_str = quant.as_str();
                let named_domain_lookup: std::collections::HashMap<String, String> =
                    named_var_names
                        .iter()
                        .cloned()
                        .zip(named_domain_src.iter().cloned())
                        .collect();
                let identity_named_mapping: std::collections::HashMap<String, String> =
                    named_var_names
                        .iter()
                        .map(|name| (name.clone(), name.clone()))
                        .collect();

                let build_named_header = |body: &str| -> Option<String> {
                    if named_var_names.is_empty() {
                        return None;
                    }

                    let (_, mut ordered_vars) =
                        replace_idents_with_order(body, &named_var_names, &identity_named_mapping);
                    if ordered_vars.is_empty() {
                        ordered_vars = named_var_names.clone();
                    }
                    let ordered_domains = ordered_vars
                        .iter()
                        .map(|name| {
                            named_domain_lookup
                                .get(name)
                                .cloned()
                                .unwrap_or_else(|| name.clone())
                        })
                        .collect::<Vec<String>>();

                    Some(format!(
                        "{} [{}] [{}]: ",
                        quant_str,
                        ordered_vars.join(","),
                        ordered_domains.join(",")
                    ))
                };

                // No explicit lists: preserve the quantified structure verbatim in header form.
                if list_domains.is_empty() {
                    let body_span = expr_pair.as_span();
                    let body_text = &spec[body_span.start()..body_span.end()];
                    if let Some(header) = build_named_header(body_text) {
                        let span = pair.as_span();
                        replacements.push((
                            span.start(),
                            span.end(),
                            format!("{header}{body_text}"),
                        ));
                    }
                    return;
                }

                // Otherwise compute the cartesian product of all explicit list domains.
                let mut combos: Vec<Vec<String>> = vec![vec![]];
                for domain in list_domains.iter() {
                    let mut next: Vec<Vec<String>> = Vec::new();
                    for combo in combos.iter() {
                        for value in domain.iter() {
                            let mut new_combo = combo.clone();
                            new_combo.push(value.clone());
                            next.push(new_combo);
                        }
                    }
                    combos = next;
                }

                let body_span = expr_pair.as_span();
                let body_text = &spec[body_span.start()..body_span.end()];

                let is_forall = matches!(quant.as_rule(), Rule::Forall);
                let join_op = if is_forall { " and " } else { " or " };

                let mut instantiated: Vec<String> = Vec::new();
                for combo in combos.iter() {
                    let mut mapping: std::collections::HashMap<String, String> =
                        std::collections::HashMap::new();
                    for (i, var) in list_var_names.iter().enumerate() {
                        mapping.insert(var.clone(), combo[i].clone());
                    }

                    let (inst_body, _) =
                        replace_idents_with_order(body_text, &list_var_names, &mapping);

                    if let Some(header) = build_named_header(&inst_body) {
                        instantiated.push(format!("({header}{inst_body})"));
                    } else {
                        instantiated.push(format!("({inst_body})"));
                    }
                }

                if !instantiated.is_empty() {
                    let body_joined = instantiated.join(join_op);
                    let span = pair.as_span();
                    replacements.push((span.start(), span.end(), body_joined));
                }
            }
            _ => {
                for inner in pair.into_inner() {
                    walk(inner, spec, replacements);
                }
            }
        }
    }

    // traverse top-level pairs
    for top in pairs {
        walk(top, spec, &mut replacements);
    }

    if replacements.is_empty() {
        return Ok(spec.to_string());
    }

    // apply replacements in reverse order
    replacements.sort_by(|a, b| b.0.cmp(&a.0));
    let mut new_spec = String::with_capacity(spec.len());
    let mut last = spec.len();
    for (start, end, repl) in replacements.iter() {
        // append tail between end and last
        if *end < last {
            new_spec.insert_str(0, &spec[*end..last]);
        }
        // insert replacement
        new_spec.insert_str(0, repl);
        last = *start;
    }
    if last > 0 {
        new_spec.insert_str(0, &spec[0..last]);
    }

    Ok(new_spec)
}

impl ExprNode {
    pub(crate) fn extract_discrete_shift(&self) -> Result<Shift, String> {
        match &self.kind {
            ExprVariant::Literal(lit) => match &lit.kind {
                LiteralKind::Number(val, unit) if unit.is_none() => val
                    .parse::<i16>()
                    .map(Shift::Discrete)
                    .map_err(|_| "invalid integer offset".to_string()),
                _ => Err("expected unit-less integer literal".to_string()),
            },
            _ => Err("expected literal expression".to_string()),
        }
    }
}

fn oorv_error_with_span(msg: &str, span: Option<SourceSpan>) -> OORVError {
    let mut diag = Diagnostic::error(msg);
    if let Some(sp) = span {
        diag = diag.add_span_with_label(sp, None::<&str>, true);
    }
    OORVError::from(diag.try_attach_source())
}
