//! Structural fault-site discovery for the CLI's actual container calls.
//! Unsupported site expressions fail closed; comments and string contents are
//! never treated as calls. This is intentionally not a Rust dataflow analyzer.
use std::collections::BTreeMap;

use syn::Expr;
use syn::visit::Visit;

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}

fn call_path(expr: &Expr) -> Option<String> {
    let Expr::Path(path) = expr else { return None };
    if path.qself.is_some() {
        return None;
    }
    Some(
        path.path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
    )
}

fn local_name(expr: &Expr) -> Option<String> {
    let Expr::Path(path) = expr else { return None };
    path.qself
        .is_none()
        .then(|| path.path.get_ident())
        .flatten()
        .map(ToString::to_string)
}

fn binding_free_condition(expr: &Expr) -> bool {
    #[derive(Default)]
    struct Condition {
        unsupported: bool,
    }
    impl<'ast> Visit<'ast> for Condition {
        fn visit_pat(&mut self, _: &'ast syn::Pat) {
            self.unsupported = true;
        }

        fn visit_expr(&mut self, expression: &'ast Expr) {
            if matches!(expression, Expr::Macro(_) | Expr::Verbatim(_)) {
                self.unsupported = true;
            } else {
                syn::visit::visit_expr(self, expression);
            }
        }
    }
    let mut condition = Condition::default();
    condition.visit_expr(expr);
    !condition.unsupported
}

#[derive(Default)]
struct Sites {
    // Freeze values at each initializer, before a later binding can shadow an
    // identifier used by an alias. Never re-resolve an initializer at its use.
    scopes: Vec<BTreeMap<String, Result<Vec<String>, String>>>,
    labels: Vec<String>,
    error: Option<String>,
    boundary: bool,
}

impl Sites {
    fn values(&self, expr: &Expr, depth: usize) -> Result<Vec<String>, String> {
        if depth > self.scopes.len() + 8 {
            return Err("cyclic site expression".into());
        }
        match expr {
            Expr::Lit(lit) => match &lit.lit {
                syn::Lit::Str(value) => Ok(vec![value.value()]),
                _ => Err("site is not a string literal".into()),
            },
            Expr::Path(_) if local_name(expr).is_some() => {
                let name = local_name(expr).unwrap();
                let value = self.scopes.iter().rev().find_map(|scope| scope.get(&name));
                match value {
                    Some(value) => value.clone(),
                    _ => Err(format!(
                        "site variable {name} has no supported immutable initializer"
                    )),
                }
            }
            Expr::Block(block) => self.block_values(&block.block, depth + 1),
            Expr::If(branch) => {
                // Values freeze before the normal visitor walks this condition.
                // A condition binding could otherwise replace an outer alias
                // while its stale value was certified for the then expression.
                if !binding_free_condition(&branch.cond) {
                    return Err("binding-bearing or opaque site condition is unsupported".into());
                }
                let mut values = self.block_values(&branch.then_branch, depth + 1)?;
                let (_, otherwise) = branch.else_branch.as_ref().ok_or("site if lacks else")?;
                values.extend(self.values(otherwise, depth + 1)?);
                Ok(values)
            }
            Expr::Paren(inner) => self.values(&inner.expr, depth + 1),
            _ => Err("unsupported site expression; extend the structural audit explicitly".into()),
        }
    }

    fn block_values(&self, block: &syn::Block, depth: usize) -> Result<Vec<String>, String> {
        match block.stmts.as_slice() {
            [syn::Stmt::Expr(expr, None)] => self.values(expr, depth),
            _ => Err("site branch must contain one value expression".into()),
        }
    }
}

impl<'ast> Visit<'ast> for Sites {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if test_only(&function.attrs) {
            return;
        }
        let old = self.boundary;
        self.boundary = function.sig.ident == "catch_child_panic_at";
        syn::visit::visit_item_fn(self, function);
        self.boundary = old;
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if !test_only(&module.attrs) {
            syn::visit::visit_item_mod(self, module);
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.scopes.push(BTreeMap::new());
        for stmt in &block.stmts {
            // Initializers see the previous binding, including `let site = site`.
            let binding = if let syn::Stmt::Local(local) = stmt
                && let syn::Pat::Ident(name) = &local.pat
            {
                let value = if name.mutability.is_none()
                    && name.by_ref.is_none()
                    && name.subpat.is_none()
                {
                    local
                        .init
                        .as_ref()
                        .ok_or_else(|| "site lacks initializer".into())
                        .and_then(|init| self.values(&init.expr, 0))
                } else {
                    Err("only plain immutable site bindings are supported".into())
                };
                Some((name.ident.to_string(), value))
            } else {
                None
            };
            self.visit_stmt(stmt);
            if let Some((name, value)) = binding {
                self.scopes.last_mut().unwrap().insert(name, value);
            }
        }
        self.scopes.pop();
    }

    fn visit_pat(&mut self, pattern: &'ast syn::Pat) {
        if matches!(pattern, syn::Pat::Macro(_) | syn::Pat::Verbatim(_)) {
            // Opaque patterns can bind names without exposing a PatIdent.
            self.error = Some("unsupported opaque pattern-bound site".into());
        } else {
            syn::visit::visit_pat(self, pattern);
        }
    }

    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        // Only the plain immutable locals above have a supported value proof.
        // Invalidate every other binder rather than inherit an outer label:
        // destructuring/typed locals, closure/function parameters, for loops,
        // if/while-let and match arms. Conservatively retaining invalidation
        // beyond a nested binder can reject a harmless shadow, never certify it.
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(
                pattern.ident.to_string(),
                Err("unsupported pattern-bound site".into()),
            );
        }
        syn::visit::visit_pat_ident(self, pattern);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let path = call_path(&call.func).unwrap_or_default();
        let index = if path == "owned_container::run" || path.ends_with("::owned_container::run") {
            Some(4)
        } else if matches!(
            path.rsplit("::").next(),
            Some("run_guarded_at" | "inject_test_fault")
        ) {
            Some(0)
        } else {
            None
        };
        if let Some(index) = index {
            let argument = call.args.iter().nth(index);
            // The separately checked boundary forwards its parameter. Its
            // callers, rather than a made-up literal here, supply the labels.
            let forwarded = self.boundary
                && path == "inject_test_fault"
                && argument.and_then(local_name).as_deref() == Some("site");
            if !forwarded {
                match argument
                    .ok_or_else(|| format!("missing site argument in {path}"))
                    .and_then(|value| self.values(value, 0))
                {
                    Ok(labels) => self.labels.extend(labels),
                    Err(error) => self.error = Some(format!("{path}: {error}")),
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

pub fn labels(text: &str) -> Result<Vec<String>, String> {
    let file = syn::parse_file(text).map_err(|error| error.to_string())?;
    let mut sites = Sites::default();
    sites.visit_file(&file);
    if let Some(error) = sites.error {
        return Err(error);
    }
    Ok(sites.labels)
}

pub fn forwards_parameter(text: &str, function: &str, index: usize, callee: &str) -> bool {
    struct Calls<'a> {
        callee: &'a str,
        parameter: &'a str,
        rebound: bool,
        arguments: Vec<Option<String>>,
    }
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if call_path(&call.func).as_deref() == Some(self.callee) {
                self.arguments.push(call.args.first().and_then(local_name));
            }
            syn::visit::visit_expr_call(self, call);
        }

        fn visit_pat(&mut self, pattern: &'ast syn::Pat) {
            if matches!(pattern, syn::Pat::Macro(_) | syn::Pat::Verbatim(_)) {
                self.rebound = true;
            } else {
                syn::visit::visit_pat(self, pattern);
            }
        }

        fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
            // Includes locals, closure arguments, and match/if-let bindings.
            // Reject even a harmless nested shadow rather than claim a full
            // interprocedural binding analysis.
            self.rebound |= pattern.ident == self.parameter;
            syn::visit::visit_pat_ident(self, pattern);
        }

        fn visit_expr_assign(&mut self, assignment: &'ast syn::ExprAssign) {
            self.rebound |= local_name(&assignment.left).as_deref() == Some(self.parameter);
            syn::visit::visit_expr_assign(self, assignment);
        }
    }
    let file = syn::parse_file(text).unwrap();
    let function = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(item) if item.sig.ident == function => Some(item),
            _ => None,
        })
        .unwrap();
    let syn::FnArg::Typed(argument) = function.sig.inputs.iter().nth(index).unwrap() else {
        return false;
    };
    let syn::Pat::Ident(parameter) = argument.pat.as_ref() else {
        return false;
    };
    if parameter.mutability.is_some() || parameter.by_ref.is_some() || parameter.subpat.is_some() {
        return false;
    }
    let name = parameter.ident.to_string();
    let mut calls = Calls {
        callee,
        parameter: &name,
        rebound: false,
        arguments: Vec::new(),
    };
    calls.visit_block(&function.block);
    !calls.rebound && calls.arguments == [Some(name)]
}

pub fn coverage<'a>(
    sources: impl IntoIterator<Item = (&'a str, &'a str)>,
    covered: &[&str],
) -> Result<(), String> {
    let mut declared = Vec::new();
    for (file, source) in sources {
        declared.extend(labels(source)?.into_iter().map(|site| (file, site)));
    }
    if declared.is_empty() {
        return Err("extracted zero fault sites".into());
    }
    let uncovered = declared
        .iter()
        .filter(|(_, site)| !covered.contains(&site.as_str()))
        .collect::<Vec<_>>();
    let stale = covered
        .iter()
        .filter(|site| !declared.iter().any(|(_, found)| found == **site))
        .collect::<Vec<_>>();
    if !uncovered.is_empty() || !stale.is_empty() {
        return Err(format!(
            "uncovered actual sites: {uncovered:?}; stale test rows: {stale:?}"
        ));
    }
    Ok(())
}
